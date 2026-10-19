//! Graph helpers that the package graph and the symbol graph share.

use std::collections::{BTreeMap, BTreeSet};

/// Returns the `(user, dependency)` edges of a reachability relation, without the edges that a
/// longer path already gives (transitive reduction). `reachable[a]` must not contain `a`.
pub(crate) fn transitive_reduction<K: Ord + Clone>(
    reachable: &BTreeMap<K, BTreeSet<K>>,
) -> Vec<(K, K)> {
    let mut edges = Vec::new();
    for (user, dependencies) in reachable {
        for dependency in dependencies {
            let has_longer_path = dependencies.iter().any(|middle| {
                middle != dependency
                    && reachable
                        .get(middle)
                        .is_some_and(|reachable| reachable.contains(dependency))
            });
            if !has_longer_path {
                edges.push((user.clone(), dependency.clone()));
            }
        }
    }
    edges
}

/// Returns the column of each node: the number of edges on the longest dependency path from the
/// node to a node with no dependencies. `edges` are `(user, dependency)` pairs.
pub(crate) fn columns<K: Ord + Clone>(nodes: &BTreeSet<K>, edges: &[(K, K)]) -> BTreeMap<K, usize> {
    let mut dependencies: BTreeMap<&K, Vec<&K>> = BTreeMap::new();
    for (user, dependency) in edges {
        dependencies.entry(user).or_default().push(dependency);
    }

    fn column_of<'a, K: Ord>(
        node: &'a K,
        dependencies: &BTreeMap<&'a K, Vec<&'a K>>,
        columns: &mut BTreeMap<&'a K, usize>,
        visiting: &mut BTreeSet<&'a K>,
    ) -> usize {
        if let Some(column) = columns.get(node) {
            return *column;
        }
        // The callers give acyclic graphs. This guard only stops a bad graph from recursing
        // forever.
        if !visiting.insert(node) {
            return 0;
        }
        let column = dependencies
            .get(node)
            .into_iter()
            .flatten()
            .map(|dependency| column_of(*dependency, dependencies, columns, visiting) + 1)
            .max()
            .unwrap_or(0);
        visiting.remove(node);
        columns.insert(node, column);
        column
    }

    let mut columns = BTreeMap::new();
    let mut visiting = BTreeSet::new();
    for node in nodes {
        column_of(node, &dependencies, &mut columns, &mut visiting);
    }
    columns
        .into_iter()
        .map(|(node, column)| (node.clone(), column))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reduction_removes_shortcuts() {
        let reachable: BTreeMap<u32, BTreeSet<u32>> = [
            (1, BTreeSet::from([2, 3])),
            (2, BTreeSet::from([3])),
            (3, BTreeSet::new()),
        ]
        .into_iter()
        .collect();
        assert_eq!(transitive_reduction(&reachable), [(1, 2), (2, 3)]);
    }

    #[test]
    fn test_columns_survive_a_cycle() {
        let nodes = BTreeSet::from(["a", "b"]);
        let columns = columns(&nodes, &[("a", "b"), ("b", "a")]);
        assert_eq!(columns.len(), 2);
    }
}
