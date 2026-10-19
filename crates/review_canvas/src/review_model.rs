use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use gpui::{Bounds, Pixels, SharedString, point, px, size};

use crate::package_graph::{self, GoPackages};

pub(crate) const NODE_WIDTH: Pixels = px(760.);
pub(crate) const NODE_HEADER_HEIGHT: Pixels = px(28.);
pub(crate) const FRAME_HEADER_HEIGHT: Pixels = px(36.);
const FRAME_PADDING: Pixels = px(16.);
const NODE_GAP: Pixels = px(24.);
const FRAME_GAP: Pixels = px(56.);
const COLUMN_GAP: Pixels = px(200.);
const OTHER_FILES_GAP: Pixels = px(120.);
const BARYCENTER_SWEEPS: usize = 4;

/// What the review model needs to know about one changed file.
#[derive(Debug, Clone)]
pub(crate) struct NodeInfo {
    /// Path relative to the repository root, with `/` separators.
    pub path: String,
    pub abs_path: PathBuf,
    pub is_generated: bool,
}

impl NodeInfo {
    pub fn is_go(&self) -> bool {
        self.path.ends_with(".go")
    }

    fn is_test(&self) -> bool {
        self.path.ends_with("_test.go")
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum ReviewOrder {
    /// Dependencies come before their users ("leaves first").
    #[default]
    BottomUp,
    /// Users come before their dependencies ("roots first").
    TopDown,
}

impl ReviewOrder {
    pub fn reversed(self) -> Self {
        match self {
            Self::BottomUp => Self::TopDown,
            Self::TopDown => Self::BottomUp,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Frame {
    pub title: SharedString,
    pub column: usize,
    /// Node indexes, in review order.
    pub nodes: Vec<usize>,
}

/// An arrow between two frames. Arrows point in the review order, from left to right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FrameEdge {
    pub from: usize,
    pub to: usize,
    /// The frame that uses the other frame. This does not change with the review order.
    pub user: usize,
    pub dependency: usize,
}

/// How a frame or an arrow relates to the active frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Relation {
    /// There is no active frame, or this item does not touch it.
    Unrelated,
    /// The active frame itself.
    Active,
    /// A package that the active frame uses, or the arrow to it.
    Dependency,
    /// A package that uses the active frame, or the arrow from it.
    User,
}

#[derive(Debug, Default)]
pub(crate) struct ReviewModel {
    /// Frames in review order: column by column, and top to bottom in a column.
    pub frames: Vec<Frame>,
    pub edges: Vec<FrameEdge>,
    /// Node indexes of the files that are not Go, in path order.
    pub other_files: Vec<usize>,
}

impl ReviewModel {
    /// Groups the nodes into package frames and orders them.
    ///
    /// Without `packages` (for example, when `go list` fails), each directory with Go changes
    /// becomes a frame, and there are no edges.
    pub fn build(
        nodes: &[NodeInfo],
        packages: Option<&GoPackages>,
        repository_root: &Path,
        order: ReviewOrder,
    ) -> Self {
        let mut other_files: Vec<usize> = Vec::new();
        // Frame key -> (title, is a package that `go list` knows, node indexes)
        let mut frames_by_key: BTreeMap<String, (String, bool, Vec<usize>)> = BTreeMap::new();
        for (index, node) in nodes.iter().enumerate() {
            if !node.is_go() {
                other_files.push(index);
                continue;
            }
            let (key, title, is_package) =
                match packages.and_then(|packages| packages.package_for_file(&node.abs_path)) {
                    Some(package) => (package.id.clone(), package.name.clone(), true),
                    None => {
                        let directory = node
                            .abs_path
                            .parent()
                            .and_then(|parent| parent.strip_prefix(repository_root).ok())
                            .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                            .filter(|relative| !relative.is_empty())
                            .unwrap_or_else(|| ".".to_string());
                        (format!("directory:{directory}"), directory, false)
                    }
                };
            frames_by_key
                .entry(key)
                .or_insert_with(|| (title, is_package, Vec::new()))
                .2
                .push(index);
        }
        other_files.sort_by(|left, right| nodes[*left].path.cmp(&nodes[*right].path));

        let changed_packages: BTreeSet<String> = frames_by_key
            .iter()
            .filter(|(_, (_, is_package, _))| *is_package)
            .map(|(key, _)| key.clone())
            .collect();
        let package_edges = packages
            .map(|packages| packages.reduced_edges(&changed_packages))
            .unwrap_or_default();
        let frame_keys: BTreeSet<String> = frames_by_key.keys().cloned().collect();
        let dependency_columns = package_graph::columns(&frame_keys, &package_edges);
        let last_column = dependency_columns.values().copied().max().unwrap_or(0);

        let keys: Vec<String> = frames_by_key.keys().cloned().collect();
        let index_of_key: BTreeMap<&str, usize> = keys
            .iter()
            .enumerate()
            .map(|(index, key)| (key.as_str(), index))
            .collect();
        let mut frames: Vec<Frame> = frames_by_key
            .into_iter()
            .map(|(key, (title, _, mut node_indexes))| {
                // Non-test files first, then tests, then generated files. Each group is in
                // path order.
                node_indexes.sort_by(|left, right| {
                    let (left, right) = (&nodes[*left], &nodes[*right]);
                    (left.is_generated, left.is_test(), &left.path).cmp(&(
                        right.is_generated,
                        right.is_test(),
                        &right.path,
                    ))
                });
                let dependency_column = dependency_columns.get(&key).copied().unwrap_or(0);
                Frame {
                    title: title.into(),
                    column: match order {
                        ReviewOrder::BottomUp => dependency_column,
                        ReviewOrder::TopDown => last_column - dependency_column,
                    },
                    nodes: node_indexes,
                }
            })
            .collect();

        let edges: Vec<(usize, usize)> = package_edges
            .iter()
            .filter_map(|(user, dependency)| {
                Some((
                    *index_of_key.get(user.as_str())?,
                    *index_of_key.get(dependency.as_str())?,
                ))
            })
            .collect();

        let rows = barycenter_rows(&frames, &edges);
        let mut frame_order: Vec<usize> = (0..frames.len()).collect();
        frame_order.sort_by(|left, right| {
            (frames[*left].column, rows[*left])
                .partial_cmp(&(frames[*right].column, rows[*right]))
                .unwrap_or(Ordering::Equal)
        });
        let mut new_index = vec![0; frames.len()];
        for (new, old) in frame_order.iter().enumerate() {
            new_index[*old] = new;
        }

        let mut edges: Vec<FrameEdge> = edges
            .into_iter()
            .map(|(user, dependency)| {
                let (user, dependency) = (new_index[user], new_index[dependency]);
                let (from, to) = match order {
                    ReviewOrder::BottomUp => (dependency, user),
                    ReviewOrder::TopDown => (user, dependency),
                };
                FrameEdge {
                    from,
                    to,
                    user,
                    dependency,
                }
            })
            .collect();
        edges.sort_by_key(|edge| (edge.from, edge.to));

        let mut ordered_frames = Vec::with_capacity(frames.len());
        let mut frames: Vec<Option<Frame>> = frames.drain(..).map(Some).collect();
        for old in frame_order {
            if let Some(frame) = frames.get_mut(old).and_then(Option::take) {
                ordered_frames.push(frame);
            }
        }

        Self {
            frames: ordered_frames,
            edges,
            other_files,
        }
    }

    /// Returns the frame that holds a node. Other files have no frame.
    pub fn frame_of_node(&self, node: usize) -> Option<usize> {
        self.frames
            .iter()
            .position(|frame| frame.nodes.contains(&node))
    }

    pub fn frame_with_title(&self, title: &str) -> Option<usize> {
        self.frames
            .iter()
            .position(|frame| frame.title.as_ref() == title)
    }

    /// Returns the relation of each frame to the active frame.
    pub fn frame_relations(&self, active: Option<usize>) -> Vec<Relation> {
        let mut relations = vec![Relation::Unrelated; self.frames.len()];
        let Some(active) = active else {
            return relations;
        };
        for edge in &self.edges {
            if edge.user == active {
                relations[edge.dependency] = Relation::Dependency;
            } else if edge.dependency == active {
                relations[edge.user] = Relation::User;
            }
        }
        if let Some(relation) = relations.get_mut(active) {
            *relation = Relation::Active;
        }
        relations
    }

    /// Returns the relation of each edge to the active frame.
    pub fn edge_relations(&self, active: Option<usize>) -> Vec<Relation> {
        self.edges
            .iter()
            .map(|edge| match active {
                Some(active) if edge.user == active => Relation::Dependency,
                Some(active) if edge.dependency == active => Relation::User,
                _ => Relation::Unrelated,
            })
            .collect()
    }

    /// Node indexes in review order. The other files come last.
    #[cfg_attr(not(test), expect(dead_code, reason = "node navigation arrives in M4"))]
    pub fn review_order(&self) -> Vec<usize> {
        self.frames
            .iter()
            .flat_map(|frame| frame.nodes.iter().copied())
            .chain(self.other_files.iter().copied())
            .collect()
    }
}

/// Returns a vertical position for each frame in its column. A barycenter sort moves each frame
/// toward the average position of its neighbors in other columns, so that fewer arrows cross.
fn barycenter_rows(frames: &[Frame], edges: &[(usize, usize)]) -> Vec<f32> {
    let mut neighbors = vec![Vec::new(); frames.len()];
    for (user, dependency) in edges {
        if let Some(list) = neighbors.get_mut(*user) {
            list.push(*dependency);
        }
        if let Some(list) = neighbors.get_mut(*dependency) {
            list.push(*user);
        }
    }

    let column_count = frames
        .iter()
        .map(|frame| frame.column + 1)
        .max()
        .unwrap_or(0);
    let mut columns: Vec<Vec<usize>> = vec![Vec::new(); column_count];
    for (index, frame) in frames.iter().enumerate() {
        columns[frame.column].push(index);
    }

    let mut rows = vec![0.; frames.len()];
    for column in &columns {
        for (row, frame) in column.iter().enumerate() {
            rows[*frame] = row as f32;
        }
    }

    for sweep in 0..BARYCENTER_SWEEPS {
        let column_indexes: Vec<usize> = if sweep % 2 == 0 {
            (1..column_count).collect()
        } else {
            (0..column_count.saturating_sub(1)).rev().collect()
        };
        for column_index in column_indexes {
            let column = &mut columns[column_index];
            let barycenter = |frame: usize| -> f32 {
                let positions: Vec<f32> = neighbors[frame]
                    .iter()
                    .filter(|neighbor| frames[**neighbor].column != column_index)
                    .map(|neighbor| rows[*neighbor])
                    .collect();
                if positions.is_empty() {
                    rows[frame]
                } else {
                    positions.iter().sum::<f32>() / positions.len() as f32
                }
            };
            let mut keyed: Vec<(f32, usize)> = column
                .iter()
                .map(|frame| (barycenter(*frame), *frame))
                .collect();
            // A stable sort keeps the title order between frames with the same barycenter.
            keyed.sort_by(|left, right| left.0.partial_cmp(&right.0).unwrap_or(Ordering::Equal));
            *column = keyed.into_iter().map(|(_, frame)| frame).collect();
            for (row, frame) in column.iter().enumerate() {
                rows[*frame] = row as f32;
            }
        }
    }
    rows
}

/// World positions of all frames and nodes.
#[derive(Debug, Default)]
pub(crate) struct Layout {
    /// Indexed by node.
    pub node_bounds: Vec<Bounds<Pixels>>,
    /// Indexed by frame.
    pub frame_bounds: Vec<Bounds<Pixels>>,
    pub other_files_bounds: Option<Bounds<Pixels>>,
}

impl Layout {
    pub fn new(model: &ReviewModel, body_heights: &[Pixels]) -> Self {
        let node_height =
            |node: usize| NODE_HEADER_HEIGHT + body_heights.get(node).copied().unwrap_or_default();
        let frame_width = NODE_WIDTH + FRAME_PADDING * 2.;
        let mut node_bounds = vec![Bounds::default(); body_heights.len()];
        let mut frame_bounds = Vec::with_capacity(model.frames.len());
        let mut column_bottoms: Vec<Pixels> = Vec::new();

        for frame in &model.frames {
            if column_bottoms.len() <= frame.column {
                column_bottoms.resize(frame.column + 1, px(0.));
            }
            let left = (frame_width + COLUMN_GAP) * frame.column as f32;
            let top = column_bottoms[frame.column];
            let mut node_top = top + FRAME_HEADER_HEIGHT;
            for node in &frame.nodes {
                let height = node_height(*node);
                if let Some(bounds) = node_bounds.get_mut(*node) {
                    *bounds = Bounds::new(
                        point(left + FRAME_PADDING, node_top),
                        size(NODE_WIDTH, height),
                    );
                }
                node_top += height + NODE_GAP;
            }
            let bottom = node_top - NODE_GAP + FRAME_PADDING;
            frame_bounds.push(Bounds::new(
                point(left, top),
                size(frame_width, bottom - top),
            ));
            column_bottoms[frame.column] = bottom + FRAME_GAP;
        }

        let other_files_bounds = (!model.other_files.is_empty()).then(|| {
            let top = column_bottoms
                .iter()
                .copied()
                .max()
                .map(|bottom| bottom - FRAME_GAP + OTHER_FILES_GAP)
                .unwrap_or_default();
            let mut node_left = FRAME_PADDING;
            let mut tallest = px(0.);
            for node in &model.other_files {
                let height = node_height(*node);
                if let Some(bounds) = node_bounds.get_mut(*node) {
                    *bounds = Bounds::new(
                        point(node_left, top + FRAME_HEADER_HEIGHT),
                        size(NODE_WIDTH, height),
                    );
                }
                node_left += NODE_WIDTH + NODE_GAP;
                tallest = tallest.max(height);
            }
            Bounds::new(
                point(px(0.), top),
                size(
                    node_left - NODE_GAP + FRAME_PADDING,
                    FRAME_HEADER_HEIGHT + tallest + FRAME_PADDING,
                ),
            )
        });

        Self {
            node_bounds,
            frame_bounds,
            other_files_bounds,
        }
    }

    pub fn content_bounds(&self) -> Option<Bounds<Pixels>> {
        self.frame_bounds
            .iter()
            .chain(self.other_files_bounds.iter())
            .copied()
            .reduce(|content, bounds| content.union(&bounds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(path: &str) -> NodeInfo {
        NodeInfo {
            path: path.to_string(),
            abs_path: Path::new("/project").join(path),
            is_generated: path.ends_with("_gen.go"),
        }
    }

    /// The changed files of the `feature/discounts` branch in `test-repos/`.
    fn discounts_nodes() -> Vec<NodeInfo> {
        [
            "README.md",
            "api/api.go",
            "config/shop.yaml",
            "internal/catalog/catalog.go",
            "internal/catalog/category.go",
            "internal/legacy/legacy.go",
            "internal/money/money.go",
            "internal/money/money_test.go",
            "internal/notify/notify.go",
            "internal/orders/discount.go",
            "internal/orders/discount_table_gen.go",
            "internal/orders/orders.go",
            "internal/orders/orders_ext_test.go",
            "internal/orders/orders_test.go",
            "internal/payments/payments.go",
            "migrations/0002_discounts.sql",
        ]
        .into_iter()
        .map(node)
        .collect()
    }

    fn shop_packages() -> GoPackages {
        GoPackages::parse(include_bytes!("../test_data/shop_go_list.json"))
            .expect("the fixture parses")
    }

    fn frame_summary(model: &ReviewModel, nodes: &[NodeInfo]) -> Vec<(usize, String, Vec<String>)> {
        model
            .frames
            .iter()
            .map(|frame| {
                (
                    frame.column,
                    frame.title.to_string(),
                    frame
                        .nodes
                        .iter()
                        .map(|node| {
                            nodes[*node]
                                .path
                                .rsplit('/')
                                .next()
                                .unwrap_or("")
                                .to_string()
                        })
                        .collect(),
                )
            })
            .collect()
    }

    fn edge_titles(model: &ReviewModel) -> Vec<(String, String)> {
        model
            .edges
            .iter()
            .map(|edge| {
                (
                    model.frames[edge.from].title.to_string(),
                    model.frames[edge.to].title.to_string(),
                )
            })
            .collect()
    }

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn test_bottom_up_model_of_the_discounts_branch() {
        let nodes = discounts_nodes();
        let packages = shop_packages();
        let model = ReviewModel::build(
            &nodes,
            Some(&packages),
            Path::new("/project"),
            ReviewOrder::BottomUp,
        );

        assert_eq!(
            frame_summary(&model, &nodes),
            vec![
                (0, "internal/legacy".into(), strings(&["legacy.go"])),
                (
                    0,
                    "internal/money".into(),
                    strings(&["money.go", "money_test.go"])
                ),
                (0, "internal/notify".into(), strings(&["notify.go"])),
                (
                    1,
                    "internal/catalog".into(),
                    strings(&["catalog.go", "category.go"])
                ),
                (1, "internal/payments".into(), strings(&["payments.go"])),
                (
                    2,
                    "internal/orders".into(),
                    strings(&[
                        "discount.go",
                        "orders.go",
                        "orders_test.go",
                        "discount_table_gen.go"
                    ])
                ),
                (
                    3,
                    "internal/orders_test".into(),
                    strings(&["orders_ext_test.go"])
                ),
                (3, "api".into(), strings(&["api.go"])),
            ]
        );

        let mut edges = edge_titles(&model);
        edges.sort();
        assert_eq!(
            edges,
            [
                ("internal/catalog", "internal/orders"),
                ("internal/money", "internal/catalog"),
                ("internal/money", "internal/payments"),
                ("internal/notify", "api"),
                ("internal/orders", "api"),
                ("internal/orders", "internal/orders_test"),
                ("internal/payments", "api"),
            ]
            .map(|(from, to)| (from.to_string(), to.to_string()))
        );
        for edge in &model.edges {
            assert!(model.frames[edge.from].column < model.frames[edge.to].column);
        }

        let other_files: Vec<&str> = model
            .other_files
            .iter()
            .map(|node| nodes[*node].path.as_str())
            .collect();
        assert_eq!(
            other_files,
            [
                "README.md",
                "config/shop.yaml",
                "migrations/0002_discounts.sql"
            ]
        );
        assert_eq!(model.review_order().len(), nodes.len());
    }

    #[test]
    fn test_top_down_mirrors_the_columns() {
        let nodes = discounts_nodes();
        let packages = shop_packages();
        let model = ReviewModel::build(
            &nodes,
            Some(&packages),
            Path::new("/project"),
            ReviewOrder::TopDown,
        );
        let column_of = |title: &str| {
            model
                .frames
                .iter()
                .find(|frame| frame.title.as_ref() == title)
                .map(|frame| frame.column)
        };
        assert_eq!(column_of("api"), Some(0));
        assert_eq!(column_of("internal/orders"), Some(1));
        assert_eq!(column_of("internal/money"), Some(3));
        assert_eq!(model.frames.first().map(|frame| frame.column), Some(0));
        for edge in &model.edges {
            assert!(model.frames[edge.from].column < model.frames[edge.to].column);
        }
        assert!(edge_titles(&model).contains(&("api".to_string(), "internal/orders".to_string())));
    }

    #[test]
    fn test_without_packages_each_directory_is_a_frame() {
        let nodes = discounts_nodes();
        let model = ReviewModel::build(&nodes, None, Path::new("/project"), ReviewOrder::BottomUp);
        assert!(model.edges.is_empty());
        assert!(model.frames.iter().all(|frame| frame.column == 0));
        let titles: Vec<&str> = model
            .frames
            .iter()
            .map(|frame| frame.title.as_ref())
            .collect();
        assert_eq!(
            titles,
            [
                "api",
                "internal/catalog",
                "internal/legacy",
                "internal/money",
                "internal/notify",
                "internal/orders",
                "internal/payments",
            ]
        );
    }

    #[test]
    fn test_layout_places_frames_in_columns_and_other_files_below() {
        let nodes = discounts_nodes();
        let packages = shop_packages();
        let model = ReviewModel::build(
            &nodes,
            Some(&packages),
            Path::new("/project"),
            ReviewOrder::BottomUp,
        );
        let body_heights = vec![px(100.); nodes.len()];
        let layout = Layout::new(&model, &body_heights);

        for (frame, bounds) in model.frames.iter().zip(&layout.frame_bounds) {
            for node in &frame.nodes {
                let node_bounds = layout.node_bounds[*node];
                assert!(
                    bounds.contains(&node_bounds.origin)
                        && bounds.contains(&node_bounds.bottom_right()),
                    "a node must be inside its frame"
                );
            }
        }
        for edge in &model.edges {
            let from = layout.frame_bounds[edge.from];
            let to = layout.frame_bounds[edge.to];
            assert!(from.right() < to.left(), "arrows go from left to right");
        }

        let other_files = layout.other_files_bounds.expect("there are other files");
        let lowest_frame = layout
            .frame_bounds
            .iter()
            .map(|bounds| bounds.bottom())
            .fold(px(0.), |lowest, bottom| lowest.max(bottom));
        assert!(other_files.top() > lowest_frame);
        for node in &model.other_files {
            assert!(other_files.contains(&layout.node_bounds[*node].origin));
        }
    }

    #[test]
    fn test_relations_follow_the_meaning_of_the_edges() {
        let nodes = discounts_nodes();
        let packages = shop_packages();
        for order in [ReviewOrder::BottomUp, ReviewOrder::TopDown] {
            let model = ReviewModel::build(&nodes, Some(&packages), Path::new("/project"), order);
            let frame = |title: &str| model.frame_with_title(title).expect("the frame exists");
            let orders = frame("internal/orders");

            let relations = model.frame_relations(Some(orders));
            let titles_with = |relation: Relation| -> Vec<String> {
                let mut titles: Vec<String> = model
                    .frames
                    .iter()
                    .zip(&relations)
                    .filter(|(_, frame_relation)| **frame_relation == relation)
                    .map(|(frame, _)| frame.title.to_string())
                    .collect();
                titles.sort();
                titles
            };
            assert_eq!(titles_with(Relation::Active), ["internal/orders"]);
            assert_eq!(titles_with(Relation::Dependency), ["internal/catalog"]);
            assert_eq!(titles_with(Relation::User), ["api", "internal/orders_test"]);

            let edge_relations = model.edge_relations(Some(orders));
            let count = |relation: Relation| {
                edge_relations
                    .iter()
                    .filter(|edge_relation| **edge_relation == relation)
                    .count()
            };
            assert_eq!(count(Relation::Dependency), 1, "{order:?}");
            assert_eq!(count(Relation::User), 2, "{order:?}");
            assert_eq!(
                count(Relation::Unrelated),
                model.edges.len() - 3,
                "{order:?}"
            );

            assert!(
                model
                    .edge_relations(None)
                    .iter()
                    .all(|relation| *relation == Relation::Unrelated)
            );
        }
    }
}
