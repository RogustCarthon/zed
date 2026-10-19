use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::graph;

/// The source of the `go-review-graph` analyzer. Zed builds it with the `go` command of the
/// project on first use.
pub(crate) const ANALYZER_SOURCES: &[(&str, &str)] = &[
    ("go.mod", include_str!("../go-review-graph/go.mod")),
    ("go.sum", include_str!("../go-review-graph/go.sum")),
    ("main.go", include_str!("../go-review-graph/main.go")),
];

#[derive(Deserialize)]
struct AnalyzerOutput {
    packages: Vec<AnalyzedPackage>,
    #[serde(default)]
    errors: Vec<String>,
}

#[derive(Deserialize)]
struct AnalyzedPackage {
    #[serde(default)]
    declarations: Vec<AnalyzedDeclaration>,
}

#[derive(Deserialize)]
struct AnalyzedDeclaration {
    id: String,
    name: String,
    file: PathBuf,
    start_line: u32,
    end_line: u32,
    #[serde(default)]
    uses: Vec<String>,
}

/// One top-level Go declaration in the head version of the code.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Declaration {
    pub id: String,
    /// The name for display, for example `Place` or `Store.Reserve`.
    pub name: String,
    pub file: PathBuf,
    /// The first line, 1-based like Go positions. It includes the doc comment.
    pub start_line: u32,
    /// The last line, inclusive.
    pub end_line: u32,
}

impl Declaration {
    fn contains_line(&self, line: u32) -> bool {
        self.start_line <= line && line <= self.end_line
    }
}

/// Groups of changed declarations and the dependency edges between them.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct SymbolUnits {
    /// Declaration indexes. A unit with more than one declaration is a dependency cycle, for
    /// example two functions that call each other.
    pub units: Vec<Vec<usize>>,
    /// `(user, dependency)` unit indexes, after the transitive reduction.
    pub edges: Vec<(usize, usize)>,
}

#[derive(Debug, Default)]
pub(crate) struct SymbolGraph {
    declarations: Vec<Declaration>,
    /// For each declaration, the declarations that it uses.
    uses: Vec<Vec<usize>>,
    /// Declaration indexes of each file, in line order.
    by_file: BTreeMap<PathBuf, Vec<usize>>,
    /// Type errors in the analyzed packages. The graph is still useful with errors.
    pub errors: Vec<String>,
}

impl SymbolGraph {
    pub fn parse(output: &[u8]) -> Result<Self> {
        let output: AnalyzerOutput =
            serde_json::from_slice(output).context("parsing the output of go-review-graph")?;

        let mut graph = Self {
            errors: output.errors,
            ..Self::default()
        };
        let mut index_of_id: BTreeMap<String, usize> = BTreeMap::new();
        let mut uses_by_id: Vec<Vec<String>> = Vec::new();
        for declaration in output
            .packages
            .into_iter()
            .flat_map(|package| package.declarations)
        {
            if index_of_id.contains_key(&declaration.id) {
                continue;
            }
            let index = graph.declarations.len();
            index_of_id.insert(declaration.id.clone(), index);
            graph
                .by_file
                .entry(declaration.file.clone())
                .or_default()
                .push(index);
            uses_by_id.push(declaration.uses);
            graph.declarations.push(Declaration {
                id: declaration.id,
                name: declaration.name,
                file: declaration.file,
                start_line: declaration.start_line,
                end_line: declaration.end_line,
            });
        }

        graph.uses = uses_by_id
            .into_iter()
            .map(|uses| {
                uses.iter()
                    .filter_map(|id| index_of_id.get(id).copied())
                    .collect()
            })
            .collect();
        for declarations in graph.by_file.values_mut() {
            declarations.sort_by_key(|index| graph.declarations[*index].start_line);
        }
        Ok(graph)
    }

    pub fn declaration(&self, index: usize) -> Option<&Declaration> {
        self.declarations.get(index)
    }

    /// Splits a change block at declaration boundaries. Each part goes to one declaration, so
    /// that a new file with many declarations does not become one node.
    ///
    /// `rows` are the head rows of the change block (0-based, `end` exclusive). Lines between
    /// two declarations go to the next declaration, and lines after the last one go to the last
    /// one. Lines before the first declaration (the package clause and the imports) give `None`.
    /// A deletion (empty `rows`) goes to the declaration that contains its position, else to the
    /// next one, else to the last one.
    pub fn split_change(&self, file: &Path, rows: Range<u32>) -> Vec<(Option<usize>, Range<u32>)> {
        let Some(declarations) = self.by_file.get(file) else {
            return vec![(None, rows)];
        };
        let declaration = |index: &usize| &self.declarations[*index];

        if rows.end <= rows.start {
            let line = rows.start + 1;
            let owner = declarations
                .iter()
                .find(|index| declaration(index).contains_line(line))
                .or_else(|| {
                    declarations
                        .iter()
                        .find(|index| declaration(index).start_line >= line)
                })
                .or_else(|| declarations.last())
                .copied();
            return vec![(owner, rows)];
        }

        let owner_of_line = |line: u32| -> Option<usize> {
            if let Some(index) = declarations
                .iter()
                .find(|index| declaration(index).contains_line(line))
            {
                return Some(*index);
            }
            match declarations
                .iter()
                .position(|index| declaration(index).start_line > line)
            {
                Some(0) => None,
                Some(next) => declarations.get(next).copied(),
                None => declarations.last().copied(),
            }
        };

        let mut parts: Vec<(Option<usize>, Range<u32>)> = Vec::new();
        for row in rows {
            let owner = owner_of_line(row + 1);
            match parts.last_mut() {
                Some((last_owner, last_rows)) if *last_owner == owner => last_rows.end = row + 1,
                _ => parts.push((owner, row..row + 1)),
            }
        }
        parts
    }

    fn reachable(&self, start: usize) -> BTreeSet<usize> {
        let mut reachable = BTreeSet::new();
        let mut stack = vec![start];
        while let Some(declaration) = stack.pop() {
            for used in self.uses.get(declaration).into_iter().flatten() {
                if reachable.insert(*used) {
                    stack.push(*used);
                }
            }
        }
        reachable
    }

    /// Groups the changed declarations into units and connects them.
    ///
    /// A changed declaration depends on another one if it reaches it through uses. The path can
    /// go through declarations without changes, also in other packages. Declarations that reach
    /// each other form one unit. The edges are transitively reduced.
    pub fn units(&self, changed: &BTreeSet<usize>) -> SymbolUnits {
        let reachable: BTreeMap<usize, BTreeSet<usize>> = changed
            .iter()
            .map(|declaration| {
                let mut reachable = self.reachable(*declaration);
                reachable.retain(|other| other != declaration && changed.contains(other));
                (*declaration, reachable)
            })
            .collect();

        let mut unit_of: BTreeMap<usize, usize> = BTreeMap::new();
        let mut units: Vec<Vec<usize>> = Vec::new();
        for declaration in changed {
            if unit_of.contains_key(declaration) {
                continue;
            }
            let unit = units.len();
            let mut members = vec![*declaration];
            members.extend(
                reachable
                    .get(declaration)
                    .into_iter()
                    .flatten()
                    .filter(|other| {
                        reachable
                            .get(other)
                            .is_some_and(|reachable| reachable.contains(declaration))
                    }),
            );
            for member in &members {
                unit_of.insert(*member, unit);
            }
            units.push(members);
        }

        let unit_reachable: BTreeMap<usize, BTreeSet<usize>> = units
            .iter()
            .enumerate()
            .map(|(unit, members)| {
                let reachable_units = members
                    .iter()
                    .flat_map(|member| reachable.get(member).into_iter().flatten())
                    .filter_map(|other| unit_of.get(other).copied())
                    .filter(|other| *other != unit)
                    .collect();
                (unit, reachable_units)
            })
            .collect();

        SymbolUnits {
            units,
            edges: graph::transitive_reduction(&unit_reachable),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODULE: &str = "example.com/shop";

    fn shop_symbols() -> SymbolGraph {
        SymbolGraph::parse(include_bytes!("../test_data/shop_symbols.json"))
            .expect("the fixture parses")
    }

    fn index_of(graph: &SymbolGraph, id: &str) -> usize {
        let id = format!("{MODULE}/{id}");
        graph
            .declarations
            .iter()
            .position(|declaration| declaration.id == id)
            .unwrap_or_else(|| panic!("{id} is in the fixture"))
    }

    /// The declarations that the `feature/discounts` branch in `test-repos/` changes.
    const CHANGED: &[&str] = &[
        "api.placeOrderRequest",
        "api.Server.ServeHTTP",
        "api.confirmation",
        "internal/catalog.Product",
        "internal/catalog.Default",
        "internal/catalog.Category",
        "internal/catalog.Drinks",
        "internal/catalog.Kitchen",
        "internal/catalog.Catalog.InCategory",
        "internal/money.Percent",
        "internal/money.Discount",
        "internal/money.TestDiscount",
        "internal/notify.Email",
        "internal/notify.Email.Notify",
        "internal/orders.Code",
        "internal/orders.applyCode",
        "internal/orders.percentFor",
        "internal/orders.discountTable",
        "internal/orders.Order",
        "internal/orders.Place",
        "internal/orders.TestPlace",
        "internal/orders.TestPlaceWithCode",
        "internal/orders_test.TestUnknownCode",
        "internal/payments.ChargeWithRetry",
    ];

    fn unit_edges(graph: &SymbolGraph) -> BTreeSet<(String, String)> {
        let changed: BTreeSet<usize> = CHANGED.iter().map(|id| index_of(graph, id)).collect();
        let units = graph.units(&changed);
        let name = |unit: usize| {
            units.units[unit]
                .iter()
                .filter_map(|declaration| graph.declaration(*declaration))
                .map(|declaration| {
                    declaration
                        .id
                        .trim_start_matches(&format!("{MODULE}/"))
                        .to_string()
                })
                .collect::<Vec<_>>()
                .join("+")
        };
        units
            .edges
            .iter()
            .map(|(user, dependency)| (name(*user), name(*dependency)))
            .collect()
    }

    fn edge(user: &str, dependency: &str) -> (String, String) {
        (user.to_string(), dependency.to_string())
    }

    #[test]
    fn test_edges_of_the_discounts_branch() {
        let edges = unit_edges(&shop_symbols());
        for expected in [
            edge("internal/orders.applyCode", "internal/money.Discount"),
            edge("internal/orders.applyCode", "internal/orders.percentFor"),
            edge(
                "internal/orders.percentFor",
                "internal/orders.discountTable",
            ),
            edge("internal/orders.discountTable", "internal/orders.Code"),
            edge("internal/orders.Order", "internal/orders.Code"),
            edge("internal/orders.Place", "internal/orders.applyCode"),
            edge("internal/orders.Place", "internal/orders.Order"),
            // Only through the unchanged `reserve` and `inventory.Store.Reserve`.
            edge("internal/orders.Place", "internal/catalog.Product"),
            edge("internal/catalog.Product", "internal/catalog.Category"),
            edge("internal/catalog.Default", "internal/catalog.Drinks"),
            edge(
                "internal/catalog.Catalog.InCategory",
                "internal/catalog.Product",
            ),
            edge("internal/money.Discount", "internal/money.Percent"),
            edge("internal/money.TestDiscount", "internal/money.Discount"),
            edge("internal/notify.Email.Notify", "internal/notify.Email"),
            edge("api.Server.ServeHTTP", "internal/orders.Place"),
            edge("api.Server.ServeHTTP", "internal/payments.ChargeWithRetry"),
            edge("api.Server.ServeHTTP", "api.confirmation"),
            edge("api.confirmation", "internal/orders.Order"),
            edge(
                "internal/orders_test.TestUnknownCode",
                "internal/orders.Place",
            ),
            edge("internal/orders.TestPlaceWithCode", "internal/orders.Place"),
        ] {
            assert!(
                edges.contains(&expected),
                "missing {expected:?} in {edges:#?}"
            );
        }

        for removed in [
            // Longer paths give these.
            edge("internal/orders.Place", "internal/money.Discount"),
            edge("internal/orders.percentFor", "internal/orders.Code"),
            edge("internal/catalog.Default", "internal/catalog.Category"),
            edge("api.Server.ServeHTTP", "internal/orders.Order"),
        ] {
            assert!(!edges.contains(&removed), "unexpected {removed:?}");
        }
    }

    #[test]
    fn test_change_blocks_split_at_declarations() {
        let graph = shop_symbols();
        let orders = Path::new("/project/internal/orders/orders.go");
        let place = index_of(&graph, "internal/orders.Place");
        let order = index_of(&graph, "internal/orders.Order");
        let start_row = |index: usize| graph.declarations[index].start_line - 1;
        let end_row = |index: usize| graph.declarations[index].end_line;

        let inside_place = start_row(place) + 2..start_row(place) + 3;
        assert_eq!(
            graph.split_change(orders, inside_place.clone()),
            [(Some(place), inside_place)]
        );

        // A change block over two declarations splits. The blank line between them goes to the
        // next declaration.
        let across = end_row(order) - 1..start_row(place) + 1;
        assert_eq!(
            graph.split_change(orders, across.clone()),
            [
                (Some(order), across.start..end_row(order)),
                (Some(place), end_row(order)..across.end),
            ]
        );

        // A deletion between two declarations goes to the next one.
        let between = end_row(order) + 1;
        assert_eq!(
            graph.split_change(orders, between..between),
            [(Some(place), between..between)]
        );

        // Imports are before all declarations.
        assert_eq!(graph.split_change(orders, 3..4), [(None, 3..4)]);
        assert_eq!(
            graph.split_change(Path::new("/project/internal/legacy/legacy.go"), 0..7),
            [(None, 0..7)]
        );
    }

    #[test]
    fn test_a_new_file_splits_into_its_declarations() {
        let graph = shop_symbols();
        let parts = graph.split_change(Path::new("/project/internal/orders/discount.go"), 0..33);
        let owners: Vec<Option<String>> = parts
            .iter()
            .map(|(owner, _)| {
                owner
                    .and_then(|index| graph.declaration(index))
                    .map(|declaration| declaration.name.clone())
            })
            .collect();
        assert_eq!(
            owners,
            [
                None,
                Some("Code".to_string()),
                Some("applyCode".to_string()),
                Some("percentFor".to_string()),
            ]
        );
        assert_eq!(parts.first().map(|(_, rows)| rows.start), Some(0));
        assert_eq!(parts.last().map(|(_, rows)| rows.end), Some(33));
        for pair in parts.windows(2) {
            assert_eq!(pair[0].1.end, pair[1].1.start, "the parts cover the block");
        }
    }

    #[test]
    fn test_mutual_uses_form_one_unit() {
        let output = br#"{"packages":[{"declarations":[
            {"id":"m.a","name":"a","file":"/m/a.go","start_line":1,"end_line":3,"uses":["m.b"]},
            {"id":"m.b","name":"b","file":"/m/a.go","start_line":5,"end_line":7,"uses":["m.a","m.c"]},
            {"id":"m.c","name":"c","file":"/m/a.go","start_line":9,"end_line":9,"uses":[]}
        ]}]}"#;
        let graph = SymbolGraph::parse(output).expect("the output parses");
        let units = graph.units(&BTreeSet::from([0, 1, 2]));
        assert_eq!(units.units, [vec![0, 1], vec![2]]);
        assert_eq!(units.edges, [(0, 1)]);
    }
}
