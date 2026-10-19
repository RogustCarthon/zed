use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result};
use serde::Deserialize;

/// The fields of `go list -json` that the review canvas reads.
pub(crate) const GO_LIST_JSON_FIELDS: &str =
    "ImportPath,Dir,Module,GoFiles,CgoFiles,TestGoFiles,XTestGoFiles,Imports,XTestImports";

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GoListPackage {
    import_path: String,
    dir: PathBuf,
    module: Option<GoListModule>,
    #[serde(default)]
    go_files: Vec<String>,
    #[serde(default)]
    cgo_files: Vec<String>,
    #[serde(default)]
    test_go_files: Vec<String>,
    #[serde(default)]
    x_test_go_files: Vec<String>,
    #[serde(default)]
    imports: Vec<String>,
    #[serde(default)]
    x_test_imports: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct GoListModule {
    path: String,
}

/// One package as the review canvas sees it. The files of an external test package
/// (`package foo_test`) make a separate package that depends on `foo`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GoPackage {
    /// The import path. External test packages add `_test`.
    pub id: String,
    /// The import path relative to the module, for display.
    pub name: String,
    pub is_external_test: bool,
    dir: PathBuf,
    files: BTreeSet<String>,
    imports: Vec<String>,
}

#[derive(Debug, Default)]
pub(crate) struct GoPackages {
    packages: Vec<GoPackage>,
}

impl GoPackages {
    /// Parses the output of `go list -e -json ./...`, which is a stream of JSON objects.
    pub fn parse(output: &[u8]) -> Result<Self> {
        let mut packages = Vec::new();
        for package in serde_json::Deserializer::from_slice(output).into_iter::<GoListPackage>() {
            let package = package.context("parsing the output of `go list`")?;
            let module_path = package.module.as_ref().map(|module| module.path.as_str());
            let name = display_name(&package.import_path, module_path);

            if !package.x_test_go_files.is_empty() {
                packages.push(GoPackage {
                    id: format!("{}_test", package.import_path),
                    name: format!("{name}_test"),
                    is_external_test: true,
                    dir: package.dir.clone(),
                    files: package.x_test_go_files.into_iter().collect(),
                    imports: package.x_test_imports,
                });
            }

            let files = package
                .go_files
                .into_iter()
                .chain(package.cgo_files)
                .chain(package.test_go_files)
                .collect();
            packages.push(GoPackage {
                id: package.import_path,
                name,
                is_external_test: false,
                dir: package.dir,
                files,
                imports: package.imports,
            });
        }
        packages.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(Self { packages })
    }

    /// Returns the package of a Go file. A file that no package lists (for example, a deleted
    /// file, or a file that a build constraint excludes) goes to the non-test package in the
    /// same directory.
    pub fn package_for_file(&self, abs_path: &Path) -> Option<&GoPackage> {
        let dir = abs_path.parent()?;
        let file_name = abs_path.file_name()?.to_str()?;
        let in_dir = || {
            self.packages
                .iter()
                .filter(move |package| package.dir == dir)
        };
        in_dir()
            .find(|package| package.files.contains(file_name))
            .or_else(|| in_dir().find(|package| !package.is_external_test))
    }

    /// Returns the dependency edges between the given packages as `(user, dependency)` pairs.
    ///
    /// An edge exists if the user reaches the dependency through imports. The path can go
    /// through packages that are not in `changed`. Edges that a longer path already gives are
    /// removed (transitive reduction), so the canvas shows fewer arrows.
    pub fn reduced_edges(&self, changed: &BTreeSet<String>) -> Vec<(String, String)> {
        let imports: BTreeMap<&str, &[String]> = self
            .packages
            .iter()
            .map(|package| (package.id.as_str(), package.imports.as_slice()))
            .collect();

        let reachable: BTreeMap<&str, BTreeSet<&str>> = changed
            .iter()
            .map(|package| {
                let mut reachable = reachable_packages(package, &imports);
                reachable.retain(|other| *other != package.as_str() && changed.contains(*other));
                (package.as_str(), reachable)
            })
            .collect();

        let mut edges = Vec::new();
        for (user, dependencies) in &reachable {
            for dependency in dependencies {
                let has_longer_path = dependencies.iter().any(|middle| {
                    middle != dependency
                        && reachable
                            .get(middle)
                            .is_some_and(|reachable| reachable.contains(dependency))
                });
                if !has_longer_path {
                    edges.push((user.to_string(), dependency.to_string()));
                }
            }
        }
        edges
    }
}

fn display_name(import_path: &str, module_path: Option<&str>) -> String {
    module_path
        .and_then(|module_path| import_path.strip_prefix(module_path))
        .and_then(|relative| relative.strip_prefix('/'))
        .unwrap_or(import_path)
        .to_string()
}

fn reachable_packages<'a>(
    start: &'a str,
    imports: &BTreeMap<&'a str, &'a [String]>,
) -> BTreeSet<&'a str> {
    let mut reachable = BTreeSet::new();
    let mut stack = vec![start];
    while let Some(package) = stack.pop() {
        for import in imports.get(package).copied().unwrap_or_default() {
            if imports.contains_key(import.as_str()) && reachable.insert(import.as_str()) {
                stack.push(import.as_str());
            }
        }
    }
    reachable
}

/// Returns the column of each package: the number of edges on the longest dependency path
/// from the package to a package with no dependencies.
pub(crate) fn columns(
    packages: &BTreeSet<String>,
    edges: &[(String, String)],
) -> BTreeMap<String, usize> {
    let mut dependencies: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (user, dependency) in edges {
        dependencies
            .entry(user.as_str())
            .or_default()
            .push(dependency.as_str());
    }

    fn column_of<'a>(
        package: &'a str,
        dependencies: &BTreeMap<&'a str, Vec<&'a str>>,
        columns: &mut BTreeMap<&'a str, usize>,
        visiting: &mut BTreeSet<&'a str>,
    ) -> usize {
        if let Some(column) = columns.get(package) {
            return *column;
        }
        // Go does not permit import cycles. This guard only stops a bad graph from recursing
        // forever.
        if !visiting.insert(package) {
            return 0;
        }
        let column = dependencies
            .get(package)
            .into_iter()
            .flatten()
            .map(|dependency| column_of(dependency, dependencies, columns, visiting) + 1)
            .max()
            .unwrap_or(0);
        visiting.remove(package);
        columns.insert(package, column);
        column
    }

    let mut columns = BTreeMap::new();
    let mut visiting = BTreeSet::new();
    for package in packages {
        column_of(package, &dependencies, &mut columns, &mut visiting);
    }
    columns
        .into_iter()
        .map(|(package, column)| (package.to_string(), column))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODULE: &str = "example.com/shop";

    fn shop_packages() -> GoPackages {
        GoPackages::parse(include_bytes!("../test_data/shop_go_list.json"))
            .expect("the fixture parses")
    }

    fn id(name: &str) -> String {
        format!("{MODULE}/{name}")
    }

    fn short(edges: &[(String, String)]) -> Vec<(String, String)> {
        let strip = |id: &str| id.trim_start_matches(&format!("{MODULE}/")).to_string();
        let mut edges: Vec<_> = edges
            .iter()
            .map(|(user, dependency)| (strip(user), strip(dependency)))
            .collect();
        edges.sort();
        edges
    }

    /// The changed packages of the `feature/discounts` branch in `test-repos/`.
    fn changed_packages() -> BTreeSet<String> {
        [
            "api",
            "internal/catalog",
            "internal/money",
            "internal/notify",
            "internal/orders",
            "internal/orders_test",
            "internal/payments",
        ]
        .into_iter()
        .map(id)
        .collect()
    }

    #[test]
    fn test_parse_splits_external_tests() {
        let packages = shop_packages();
        let orders = packages
            .package_for_file(Path::new("/project/internal/orders/orders_test.go"))
            .expect("internal tests belong to the package");
        assert_eq!(orders.id, id("internal/orders"));
        assert_eq!(orders.name, "internal/orders");

        let external = packages
            .package_for_file(Path::new("/project/internal/orders/orders_ext_test.go"))
            .expect("external tests have a package");
        assert_eq!(external.id, id("internal/orders_test"));
        assert_eq!(external.name, "internal/orders_test");
        assert!(external.is_external_test);
    }

    #[test]
    fn test_unlisted_files_go_to_the_package_of_their_directory() {
        let packages = shop_packages();
        let package = packages
            .package_for_file(Path::new("/project/internal/orders/deleted.go"))
            .expect("the directory has a package");
        assert_eq!(package.id, id("internal/orders"));
        assert_eq!(
            packages.package_for_file(Path::new("/project/internal/legacy/legacy.go")),
            None
        );
    }

    #[test]
    fn test_reduced_edges_of_the_discounts_branch() {
        let edges = shop_packages().reduced_edges(&changed_packages());
        let expected: Vec<(String, String)> = [
            ("api", "internal/notify"),
            ("api", "internal/orders"),
            ("api", "internal/payments"),
            ("internal/catalog", "internal/money"),
            // Only through internal/inventory, which has no changes.
            ("internal/orders", "internal/catalog"),
            ("internal/orders_test", "internal/orders"),
            ("internal/payments", "internal/money"),
        ]
        .into_iter()
        .map(|(user, dependency)| (user.to_string(), dependency.to_string()))
        .collect();
        assert_eq!(short(&edges), expected);
    }

    #[test]
    fn test_columns_of_the_discounts_branch() {
        let changed = changed_packages();
        let edges = shop_packages().reduced_edges(&changed);
        let columns = columns(&changed, &edges);
        let column = |name: &str| columns.get(&id(name)).copied();
        assert_eq!(column("internal/money"), Some(0));
        assert_eq!(column("internal/notify"), Some(0));
        assert_eq!(column("internal/catalog"), Some(1));
        assert_eq!(column("internal/payments"), Some(1));
        assert_eq!(column("internal/orders"), Some(2));
        assert_eq!(column("internal/orders_test"), Some(3));
        assert_eq!(column("api"), Some(3));
    }

    #[test]
    fn test_columns_survive_a_cycle() {
        let packages: BTreeSet<String> = ["a", "b"].into_iter().map(String::from).collect();
        let edges = vec![
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "a".to_string()),
        ];
        let columns = columns(&packages, &edges);
        assert_eq!(columns.len(), 2);
    }
}
