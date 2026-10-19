use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    path::{Path, PathBuf},
};

use gpui::{Bounds, Pixels, SharedString, point, px, size};

use crate::{
    graph,
    package_graph::{self, GoPackages},
    symbol_graph::SymbolGraph,
};

pub(crate) const NODE_WIDTH: Pixels = px(760.);
pub(crate) const NODE_HEADER_HEIGHT: Pixels = px(28.);
pub(crate) const FRAME_HEADER_HEIGHT: Pixels = px(36.);
const FRAME_PADDING: Pixels = px(16.);
const NODE_GAP: Pixels = px(24.);
const SUB_COLUMN_GAP: Pixels = px(96.);
const FRAME_GAP: Pixels = px(56.);
const COLUMN_GAP: Pixels = px(200.);
const OTHER_FILES_GAP: Pixels = px(120.);
const BARYCENTER_SWEEPS: usize = 4;

/// One changed file and the head rows of its change blocks.
#[derive(Debug, Clone)]
pub(crate) struct ChangedFile {
    /// Path relative to the repository root, with `/` separators.
    pub path: String,
    pub abs_path: PathBuf,
    pub is_generated: bool,
    /// The head rows of each change block (0-based, `end` exclusive), in file order. An empty
    /// range is a deletion.
    pub change_blocks: Vec<Range<u32>>,
}

impl ChangedFile {
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

/// A part of a change block. A change block that covers more than one declaration splits into
/// parts, and each part can be in a different node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockPart {
    pub change_block: usize,
    pub rows: Range<u32>,
}

impl BlockPart {
    fn sort_key(&self) -> (u32, u32, usize) {
        (self.rows.start, self.rows.end, self.change_block)
    }
}

/// Parts sort in file order.
impl Ord for BlockPart {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

impl PartialOrd for BlockPart {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModelNode {
    pub file: usize,
    /// In file order.
    pub parts: Vec<BlockPart>,
    /// The names of the declarations in the node, for display.
    pub declarations: Vec<SharedString>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Frame {
    pub title: SharedString,
    pub column: usize,
    /// Node indexes of each sub-column, top to bottom. Sub-columns go left to right.
    pub sub_columns: Vec<Vec<usize>>,
}

impl Frame {
    /// Node indexes in review order: sub-column by sub-column, top to bottom.
    pub fn nodes(&self) -> impl Iterator<Item = usize> + '_ {
        self.sub_columns.iter().flatten().copied()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Endpoint {
    Node(usize),
    /// Only when there is no symbol graph: the arrows connect package frames.
    Frame(usize),
}

/// An arrow. Arrows point in the review order, from left to right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModelEdge {
    pub from: Endpoint,
    pub to: Endpoint,
    /// The end that uses the other end. This does not change with the review order.
    pub user: Endpoint,
    pub dependency: Endpoint,
}

/// The item that the user looks at: the node with the focused editor, or a selected frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Active {
    Node(usize),
    Frame(usize),
}

/// How a frame, a node, or an arrow relates to the active item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Relation {
    /// There is no active item, or this item does not touch it.
    Unrelated,
    /// The active item itself, or an arrow inside the active frame.
    Active,
    /// Something that the active item uses, or the arrow to it.
    Dependency,
    /// Something that uses the active item, or the arrow from it.
    User,
}

#[derive(Debug, Default)]
pub(crate) struct ReviewModel {
    /// Sorted by file and first row, so that the same changes give the same nodes in both review
    /// orders. The canvas keeps one editor for each node.
    pub nodes: Vec<ModelNode>,
    /// Frames in review order: column by column, and top to bottom in a column.
    pub frames: Vec<Frame>,
    pub edges: Vec<ModelEdge>,
    /// Nodes of the files that are not Go, in path order.
    pub other_files: Vec<usize>,
}

/// A group of change block parts that moves as one in the layout: the parts of one symbol unit
/// (one declaration, or declarations that use each other), or the parts of a file outside all
/// declarations.
#[derive(Debug, Default)]
struct Item {
    frame_key: String,
    /// `(file, part)` pairs.
    parts: Vec<(usize, BlockPart)>,
    /// `(file, start line, name)` of each declaration.
    declarations: Vec<(usize, u32, String)>,
}

/// Where a node goes: `(frame, sub-column, file)`. Other files have no placement.
type Placement = (usize, usize, usize);

impl ReviewModel {
    /// Builds the frames, nodes, and edges.
    ///
    /// With `symbols`, nodes hold the changed declarations of one file in one sub-column of a
    /// frame, and the edges connect nodes. Without `symbols`, each file is one node, and the
    /// edges connect package frames. Without `packages` (for example, when `go list` fails),
    /// each directory with Go changes is a frame, and there are no edges.
    pub fn build(
        files: &[ChangedFile],
        packages: Option<&GoPackages>,
        symbols: Option<&SymbolGraph>,
        repository_root: &Path,
        order: ReviewOrder,
    ) -> Self {
        // Frame key -> (title, is a package that `go list` knows).
        let mut frame_info: BTreeMap<String, (String, bool)> = BTreeMap::new();
        let mut frame_key_of_file: BTreeMap<usize, String> = BTreeMap::new();
        for (index, file) in files.iter().enumerate() {
            if !file.is_go() {
                continue;
            }
            let (key, title, is_package) =
                match packages.and_then(|packages| packages.package_for_file(&file.abs_path)) {
                    Some(package) => (package.id.clone(), package.name.clone(), true),
                    None => {
                        let directory = directory_title(&file.abs_path, repository_root);
                        (format!("directory:{directory}"), directory, false)
                    }
                };
            frame_info.entry(key.clone()).or_insert((title, is_package));
            frame_key_of_file.insert(index, key);
        }

        let (items, item_edges) = match symbols {
            Some(symbols) => symbol_items(files, &frame_key_of_file, symbols),
            None => (file_items(files, &frame_key_of_file), Vec::new()),
        };

        // Package columns.
        let changed_packages: BTreeSet<String> = frame_info
            .iter()
            .filter(|(_, (_, is_package))| *is_package)
            .map(|(key, _)| key.clone())
            .collect();
        let package_edges = packages
            .map(|packages| packages.reduced_edges(&changed_packages))
            .unwrap_or_default();
        let frame_keys: Vec<String> = frame_info.keys().cloned().collect();
        let frame_key_set: BTreeSet<String> = frame_keys.iter().cloned().collect();
        let dependency_columns = package_graph::columns(&frame_key_set, &package_edges);
        let last_column = dependency_columns.values().copied().max().unwrap_or(0);
        let frame_columns: Vec<usize> = frame_keys
            .iter()
            .map(|key| {
                let column = dependency_columns.get(key).copied().unwrap_or(0);
                match order {
                    ReviewOrder::BottomUp => column,
                    ReviewOrder::TopDown => last_column - column,
                }
            })
            .collect();
        let frame_of_key: BTreeMap<&str, usize> = frame_keys
            .iter()
            .enumerate()
            .map(|(index, key)| (key.as_str(), index))
            .collect();

        // Sub-columns of the items in their frame, from the edges inside the frame.
        let mut item_sub_column = vec![0; items.len()];
        for frame_key in &frame_keys {
            let frame_items: BTreeSet<usize> = (0..items.len())
                .filter(|item| items[*item].frame_key == *frame_key)
                .collect();
            let inner_edges: Vec<(usize, usize)> = item_edges
                .iter()
                .filter(|(user, dependency)| {
                    frame_items.contains(user) && frame_items.contains(dependency)
                })
                .copied()
                .collect();
            let columns = graph::columns(&frame_items, &inner_edges);
            let last = columns.values().copied().max().unwrap_or(0);
            for (item, column) in columns {
                item_sub_column[item] = match order {
                    ReviewOrder::BottomUp => column,
                    ReviewOrder::TopDown => last - column,
                };
            }
        }

        // Nodes: the items of one file in one frame and one sub-column. The grouping does not
        // depend on the review order, because mirroring keeps the partition.
        let mut groups: BTreeMap<Placement, (Vec<BlockPart>, Vec<(u32, String)>)> = BTreeMap::new();
        let mut item_groups: Vec<BTreeSet<Placement>> = vec![BTreeSet::new(); items.len()];
        for (index, item) in items.iter().enumerate() {
            let frame = frame_of_key
                .get(item.frame_key.as_str())
                .copied()
                .unwrap_or(0);
            for (file, part) in &item.parts {
                let placement = (frame, item_sub_column[index], *file);
                groups.entry(placement).or_default().0.push(part.clone());
                item_groups[index].insert(placement);
            }
            for (file, line, name) in &item.declarations {
                let placement = (frame, item_sub_column[index], *file);
                groups
                    .entry(placement)
                    .or_default()
                    .1
                    .push((*line, name.clone()));
            }
        }

        let mut nodes: Vec<(ModelNode, Option<Placement>)> = groups
            .into_iter()
            .filter(|(_, (parts, _))| !parts.is_empty())
            .map(|(placement, (mut parts, mut declarations))| {
                parts.sort();
                declarations.sort();
                declarations.dedup();
                (
                    ModelNode {
                        file: placement.2,
                        parts,
                        declarations: declarations
                            .into_iter()
                            .map(|(_, name)| name.into())
                            .collect(),
                    },
                    Some(placement),
                )
            })
            .collect();
        for (index, file) in files.iter().enumerate() {
            if !file.is_go() {
                nodes.push((
                    ModelNode {
                        file: index,
                        parts: whole_file_parts(file),
                        declarations: Vec::new(),
                    },
                    None,
                ));
            }
        }
        nodes.sort_by(|(left, _), (right, _)| {
            (&files[left.file].path, left.parts.first())
                .cmp(&(&files[right.file].path, right.parts.first()))
        });

        let node_of_placement: BTreeMap<Placement, usize> = nodes
            .iter()
            .enumerate()
            .filter_map(|(index, (_, placement))| placement.map(|placement| (placement, index)))
            .collect();

        // Node edges from the item edges, reduced again, because two items can fall into the
        // same node.
        let mut node_uses: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
        for (user, dependency) in &item_edges {
            for user_placement in &item_groups[*user] {
                for dependency_placement in &item_groups[*dependency] {
                    let (Some(user_node), Some(dependency_node)) = (
                        node_of_placement.get(user_placement),
                        node_of_placement.get(dependency_placement),
                    ) else {
                        continue;
                    };
                    if user_node != dependency_node {
                        node_uses
                            .entry(*user_node)
                            .or_default()
                            .insert(*dependency_node);
                    }
                }
            }
        }
        let node_reachable: BTreeMap<usize, BTreeSet<usize>> = (0..nodes.len())
            .map(|node| {
                let mut reachable = BTreeSet::new();
                let mut stack = vec![node];
                while let Some(current) = stack.pop() {
                    for next in node_uses.get(&current).into_iter().flatten() {
                        if *next != node && reachable.insert(*next) {
                            stack.push(*next);
                        }
                    }
                }
                (node, reachable)
            })
            .collect();
        let node_edges = graph::transitive_reduction(&node_reachable);

        // Vertical order of the frames in each column.
        let frame_edges: Vec<(usize, usize)> = package_edges
            .iter()
            .filter_map(|(user, dependency)| {
                Some((
                    *frame_of_key.get(user.as_str())?,
                    *frame_of_key.get(dependency.as_str())?,
                ))
            })
            .collect();
        let frame_rows = barycenter_rows(&frame_columns, &frame_edges);
        let mut frame_order: Vec<usize> = (0..frame_keys.len()).collect();
        frame_order.sort_by(|left, right| {
            (frame_columns[*left], frame_rows[*left])
                .partial_cmp(&(frame_columns[*right], frame_rows[*right]))
                .unwrap_or(Ordering::Equal)
        });
        let mut new_frame_index = vec![0; frame_keys.len()];
        for (new, old) in frame_order.iter().enumerate() {
            new_frame_index[*old] = new;
        }

        // Vertical order of the nodes in each sub-column.
        let mut frames: Vec<Frame> = Vec::with_capacity(frame_order.len());
        for old_frame in &frame_order {
            let (title, _) = &frame_info[&frame_keys[*old_frame]];
            let mut frame_nodes: Vec<usize> = nodes
                .iter()
                .enumerate()
                .filter(|(_, (_, placement))| {
                    placement.is_some_and(|(frame, _, _)| frame == *old_frame)
                })
                .map(|(index, _)| index)
                .collect();
            // The start order for the barycenter sort: normal files first, then tests, then
            // generated files, each in path order.
            frame_nodes.sort_by(|left, right| {
                let key = |node: usize| {
                    let file = &files[nodes[node].0.file];
                    (
                        file.is_generated,
                        file.is_test(),
                        &file.path,
                        nodes[node].0.parts.first().cloned(),
                    )
                };
                key(*left).cmp(&key(*right))
            });
            let local_index: BTreeMap<usize, usize> = frame_nodes
                .iter()
                .enumerate()
                .map(|(local, node)| (*node, local))
                .collect();
            let local_columns: Vec<usize> = frame_nodes
                .iter()
                .map(|node| nodes[*node].1.map_or(0, |(_, sub_column, _)| sub_column))
                .collect();
            let local_edges: Vec<(usize, usize)> = node_edges
                .iter()
                .filter_map(|(user, dependency)| {
                    Some((*local_index.get(user)?, *local_index.get(dependency)?))
                })
                .collect();
            let rows = barycenter_rows(&local_columns, &local_edges);
            let sub_column_count = local_columns
                .iter()
                .map(|column| column + 1)
                .max()
                .unwrap_or(0);
            let mut sub_columns: Vec<Vec<(f32, usize)>> = vec![Vec::new(); sub_column_count];
            for (local, node) in frame_nodes.iter().enumerate() {
                sub_columns[local_columns[local]].push((rows[local], *node));
            }
            frames.push(Frame {
                title: title.clone().into(),
                column: frame_columns[*old_frame],
                sub_columns: sub_columns
                    .into_iter()
                    .map(|mut column| {
                        column.sort_by(|left, right| {
                            left.0.partial_cmp(&right.0).unwrap_or(Ordering::Equal)
                        });
                        column.into_iter().map(|(_, node)| node).collect()
                    })
                    .collect(),
            });
        }

        let make_edge = |user: Endpoint, dependency: Endpoint| {
            let (from, to) = match order {
                ReviewOrder::BottomUp => (dependency, user),
                ReviewOrder::TopDown => (user, dependency),
            };
            ModelEdge {
                from,
                to,
                user,
                dependency,
            }
        };
        let mut edges: Vec<ModelEdge> = if symbols.is_some() {
            node_edges
                .iter()
                .map(|(user, dependency)| {
                    make_edge(Endpoint::Node(*user), Endpoint::Node(*dependency))
                })
                .collect()
        } else {
            frame_edges
                .iter()
                .map(|(user, dependency)| {
                    make_edge(
                        Endpoint::Frame(new_frame_index[*user]),
                        Endpoint::Frame(new_frame_index[*dependency]),
                    )
                })
                .collect()
        };
        edges.sort_by_key(|edge| (edge.from, edge.to));

        let other_files: Vec<usize> = nodes
            .iter()
            .enumerate()
            .filter(|(_, (_, placement))| placement.is_none())
            .map(|(index, _)| index)
            .collect();

        Self {
            nodes: nodes.into_iter().map(|(node, _)| node).collect(),
            frames,
            edges,
            other_files,
        }
    }

    /// Returns the frame that holds a node. Other files have no frame.
    pub fn frame_of_node(&self, node: usize) -> Option<usize> {
        self.frames
            .iter()
            .position(|frame| frame.nodes().any(|other| other == node))
    }

    pub fn frame_with_title(&self, title: &str) -> Option<usize> {
        self.frames
            .iter()
            .position(|frame| frame.title.as_ref() == title)
    }

    fn touches(&self, endpoint: Endpoint, active: Active) -> bool {
        match (endpoint, active) {
            (Endpoint::Node(node), Active::Node(active)) => node == active,
            (Endpoint::Node(node), Active::Frame(active)) => {
                self.frame_of_node(node) == Some(active)
            }
            (Endpoint::Frame(frame), Active::Frame(active)) => frame == active,
            (Endpoint::Frame(frame), Active::Node(active)) => {
                self.frame_of_node(active) == Some(frame)
            }
        }
    }

    /// Returns the relation of each edge to the active item.
    pub fn edge_relations(&self, active: Option<Active>) -> Vec<Relation> {
        self.edges
            .iter()
            .map(|edge| {
                let Some(active) = active else {
                    return Relation::Unrelated;
                };
                match (
                    self.touches(edge.user, active),
                    self.touches(edge.dependency, active),
                ) {
                    (true, true) => Relation::Active,
                    (true, false) => Relation::Dependency,
                    (false, true) => Relation::User,
                    (false, false) => Relation::Unrelated,
                }
            })
            .collect()
    }

    /// The ends of the edges that relate to the active item, as `(end, relation)` pairs.
    fn related_ends(&self, active: Active) -> Vec<(Endpoint, Relation)> {
        self.edges
            .iter()
            .zip(self.edge_relations(Some(active)))
            .filter_map(|(edge, relation)| match relation {
                Relation::Dependency => Some((edge.dependency, relation)),
                Relation::User => Some((edge.user, relation)),
                Relation::Active | Relation::Unrelated => None,
            })
            .collect()
    }

    /// Returns the relation of each node to the active item.
    pub fn node_relations(&self, active: Option<Active>) -> Vec<Relation> {
        let mut relations = vec![Relation::Unrelated; self.nodes.len()];
        let Some(active) = active else {
            return relations;
        };
        for (end, relation) in self.related_ends(active) {
            if let Endpoint::Node(node) = end
                && let Some(slot) = relations.get_mut(node)
                && *slot == Relation::Unrelated
            {
                *slot = relation;
            }
        }
        let active_nodes: Vec<usize> = match active {
            Active::Node(node) => vec![node],
            Active::Frame(frame) => self
                .frames
                .get(frame)
                .map(|frame| frame.nodes().collect())
                .unwrap_or_default(),
        };
        for node in active_nodes {
            if let Some(slot) = relations.get_mut(node) {
                *slot = Relation::Active;
            }
        }
        relations
    }

    /// Returns the relation of each frame to the active item.
    pub fn frame_relations(&self, active: Option<Active>) -> Vec<Relation> {
        let mut relations = vec![Relation::Unrelated; self.frames.len()];
        let Some(active) = active else {
            return relations;
        };
        for (end, relation) in self.related_ends(active) {
            let frame = match end {
                Endpoint::Node(node) => self.frame_of_node(node),
                Endpoint::Frame(frame) => Some(frame),
            };
            if let Some(slot) = frame.and_then(|frame| relations.get_mut(frame))
                && *slot == Relation::Unrelated
            {
                *slot = relation;
            }
        }
        let active_frame = match active {
            Active::Node(node) => self.frame_of_node(node),
            Active::Frame(frame) => Some(frame),
        };
        if let Some(slot) = active_frame.and_then(|frame| relations.get_mut(frame)) {
            *slot = Relation::Active;
        }
        relations
    }

    /// Node indexes in review order. The other files come last.
    #[cfg_attr(not(test), expect(dead_code, reason = "node navigation arrives in M4"))]
    pub fn review_order(&self) -> Vec<usize> {
        self.frames
            .iter()
            .flat_map(|frame| frame.nodes())
            .chain(self.other_files.iter().copied())
            .collect()
    }
}

fn directory_title(abs_path: &Path, repository_root: &Path) -> String {
    abs_path
        .parent()
        .and_then(|parent| parent.strip_prefix(repository_root).ok())
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .filter(|relative| !relative.is_empty())
        .unwrap_or_else(|| ".".to_string())
}

fn whole_file_parts(file: &ChangedFile) -> Vec<BlockPart> {
    file.change_blocks
        .iter()
        .enumerate()
        .map(|(change_block, rows)| BlockPart {
            change_block,
            rows: rows.clone(),
        })
        .collect()
}

/// One item for each Go file, with all of its change blocks. Generated files sort last.
fn file_items(files: &[ChangedFile], frame_key_of_file: &BTreeMap<usize, String>) -> Vec<Item> {
    let mut items: Vec<(usize, Item)> = frame_key_of_file
        .iter()
        .map(|(file, frame_key)| {
            (
                *file,
                Item {
                    frame_key: frame_key.clone(),
                    parts: whole_file_parts(&files[*file])
                        .into_iter()
                        .map(|part| (*file, part))
                        .collect(),
                    declarations: Vec::new(),
                },
            )
        })
        .collect();
    items.sort_by_key(|(file, _)| files[*file].is_generated);
    items.into_iter().map(|(_, item)| item).collect()
}

/// Items from the symbol graph: one for each unit of changed declarations, and one for each file
/// with changes outside all declarations. Returns the `(user, dependency)` edges between items.
fn symbol_items(
    files: &[ChangedFile],
    frame_key_of_file: &BTreeMap<usize, String>,
    symbols: &SymbolGraph,
) -> (Vec<Item>, Vec<(usize, usize)>) {
    let mut declaration_parts: BTreeMap<usize, Vec<(usize, BlockPart)>> = BTreeMap::new();
    let mut file_level_parts: BTreeMap<usize, Vec<BlockPart>> = BTreeMap::new();
    for file_index in frame_key_of_file.keys() {
        let file = &files[*file_index];
        for (change_block, rows) in file.change_blocks.iter().enumerate() {
            for (owner, rows) in symbols.split_change(&file.abs_path, rows.clone()) {
                let part = BlockPart { change_block, rows };
                match owner {
                    Some(declaration) => declaration_parts
                        .entry(declaration)
                        .or_default()
                        .push((*file_index, part)),
                    None => file_level_parts.entry(*file_index).or_default().push(part),
                }
            }
        }
    }

    let changed: BTreeSet<usize> = declaration_parts.keys().copied().collect();
    let units = symbols.units(&changed);
    let file_of_path: BTreeMap<&Path, usize> = files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.abs_path.as_path(), index))
        .collect();

    let mut items = Vec::new();
    for members in &units.units {
        let mut item = Item::default();
        for member in members {
            let Some(declaration) = symbols.declaration(*member) else {
                continue;
            };
            let Some(file) = file_of_path.get(declaration.file.as_path()).copied() else {
                continue;
            };
            if item.frame_key.is_empty()
                && let Some(frame_key) = frame_key_of_file.get(&file)
            {
                item.frame_key = frame_key.clone();
            }
            item.parts
                .extend(declaration_parts.get(member).into_iter().flatten().cloned());
            item.declarations
                .push((file, declaration.start_line, declaration.name.clone()));
        }
        items.push(item);
    }
    for (file, parts) in file_level_parts {
        if let Some(frame_key) = frame_key_of_file.get(&file) {
            items.push(Item {
                frame_key: frame_key.clone(),
                parts: parts.into_iter().map(|part| (file, part)).collect(),
                declarations: Vec::new(),
            });
        }
    }
    (items, units.edges)
}

/// Returns a vertical position for each item in its column. A barycenter sort moves each item
/// toward the average position of its neighbors in other columns, so that fewer arrows cross.
/// The input order is the start order in each column.
fn barycenter_rows(columns_of: &[usize], edges: &[(usize, usize)]) -> Vec<f32> {
    let mut neighbors = vec![Vec::new(); columns_of.len()];
    for (user, dependency) in edges {
        if let Some(list) = neighbors.get_mut(*user) {
            list.push(*dependency);
        }
        if let Some(list) = neighbors.get_mut(*dependency) {
            list.push(*user);
        }
    }

    let column_count = columns_of
        .iter()
        .map(|column| column + 1)
        .max()
        .unwrap_or(0);
    let mut columns: Vec<Vec<usize>> = vec![Vec::new(); column_count];
    for (index, column) in columns_of.iter().enumerate() {
        columns[*column].push(index);
    }

    let mut rows = vec![0.; columns_of.len()];
    for column in &columns {
        for (row, item) in column.iter().enumerate() {
            rows[*item] = row as f32;
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
            let barycenter = |item: usize| -> f32 {
                let positions: Vec<f32> = neighbors[item]
                    .iter()
                    .filter(|neighbor| columns_of[**neighbor] != column_index)
                    .map(|neighbor| rows[*neighbor])
                    .collect();
                if positions.is_empty() {
                    rows[item]
                } else {
                    positions.iter().sum::<f32>() / positions.len() as f32
                }
            };
            let mut keyed: Vec<(f32, usize)> = column
                .iter()
                .map(|item| (barycenter(*item), *item))
                .collect();
            // A stable sort keeps the start order between items with the same barycenter.
            keyed.sort_by(|left, right| left.0.partial_cmp(&right.0).unwrap_or(Ordering::Equal));
            *column = keyed.into_iter().map(|(_, item)| item).collect();
            for (row, item) in column.iter().enumerate() {
                rows[*item] = row as f32;
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
        let frame_width = |frame: &Frame| {
            let sub_columns = frame.sub_columns.len().max(1) as f32;
            NODE_WIDTH * sub_columns + SUB_COLUMN_GAP * (sub_columns - 1.) + FRAME_PADDING * 2.
        };

        let column_count = model
            .frames
            .iter()
            .map(|frame| frame.column + 1)
            .max()
            .unwrap_or(0);
        let mut column_widths = vec![px(0.); column_count];
        for frame in &model.frames {
            column_widths[frame.column] = column_widths[frame.column].max(frame_width(frame));
        }
        let mut column_lefts = Vec::with_capacity(column_count);
        let mut left = px(0.);
        for width in &column_widths {
            column_lefts.push(left);
            left += *width + COLUMN_GAP;
        }

        let mut node_bounds = vec![Bounds::default(); model.nodes.len()];
        let mut frame_bounds = Vec::with_capacity(model.frames.len());
        let mut column_bottoms = vec![px(0.); column_count];
        for frame in &model.frames {
            let left = column_lefts[frame.column];
            let top = column_bottoms[frame.column];
            let body_top = top + FRAME_HEADER_HEIGHT;
            let mut tallest = px(0.);
            for (sub_column, nodes) in frame.sub_columns.iter().enumerate() {
                let node_left =
                    left + FRAME_PADDING + (NODE_WIDTH + SUB_COLUMN_GAP) * sub_column as f32;
                let mut node_top = body_top;
                for node in nodes {
                    let height = node_height(*node);
                    if let Some(bounds) = node_bounds.get_mut(*node) {
                        *bounds = Bounds::new(point(node_left, node_top), size(NODE_WIDTH, height));
                    }
                    node_top += height + NODE_GAP;
                }
                if !nodes.is_empty() {
                    tallest = tallest.max(node_top - NODE_GAP - body_top);
                }
            }
            let bottom = body_top + tallest + FRAME_PADDING;
            frame_bounds.push(Bounds::new(
                point(left, top),
                size(frame_width(frame), bottom - top),
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

    pub fn endpoint_bounds(&self, endpoint: Endpoint) -> Option<Bounds<Pixels>> {
        match endpoint {
            Endpoint::Node(node) => self.node_bounds.get(node).copied(),
            Endpoint::Frame(frame) => self.frame_bounds.get(frame).copied(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The changed files of the `feature/discounts` branch in `test-repos/`, with the head rows
    /// of their change blocks (from `git diff -U0`).
    const DISCOUNTS_FILES: &[(&str, &[(u32, u32)])] = &[
        ("README.md", &[(5, 10)]),
        (
            "api/api.go",
            &[(24, 25), (34, 35), (39, 40), (43, 44), (46, 53)],
        ),
        ("config/shop.yaml", &[(5, 9)]),
        ("internal/catalog/catalog.go", &[(7, 11), (25, 28)]),
        ("internal/catalog/category.go", &[(0, 24)]),
        ("internal/legacy/legacy.go", &[(0, 0)]),
        ("internal/money/money.go", &[(26, 34)]),
        ("internal/money/money_test.go", &[(0, 12)]),
        ("internal/notify/notify.go", &[(3, 8), (21, 32)]),
        ("internal/orders/discount.go", &[(0, 33)]),
        ("internal/orders/discount_table_gen.go", &[(0, 11)]),
        ("internal/orders/orders.go", &[(18, 19), (22, 24), (28, 33)]),
        ("internal/orders/orders_ext_test.go", &[(0, 17)]),
        ("internal/orders/orders_test.go", &[(11, 12), (19, 30)]),
        ("internal/payments/payments.go", &[(21, 34)]),
        ("migrations/0002_discounts.sql", &[(0, 1)]),
    ];

    fn discounts_files() -> Vec<ChangedFile> {
        DISCOUNTS_FILES
            .iter()
            .map(|(path, blocks)| ChangedFile {
                path: path.to_string(),
                abs_path: Path::new("/project").join(path),
                is_generated: path.ends_with("_gen.go"),
                change_blocks: blocks.iter().map(|(start, end)| *start..*end).collect(),
            })
            .collect()
    }

    fn shop_packages() -> GoPackages {
        GoPackages::parse(include_bytes!("../test_data/shop_go_list.json"))
            .expect("the fixture parses")
    }

    fn shop_symbols() -> SymbolGraph {
        SymbolGraph::parse(include_bytes!("../test_data/shop_symbols.json"))
            .expect("the fixture parses")
    }

    fn build(order: ReviewOrder, with_symbols: bool) -> (Vec<ChangedFile>, ReviewModel) {
        let files = discounts_files();
        let packages = shop_packages();
        let symbols = shop_symbols();
        let model = ReviewModel::build(
            &files,
            Some(&packages),
            with_symbols.then_some(&symbols),
            Path::new("/project"),
            order,
        );
        (files, model)
    }

    fn file_name(files: &[ChangedFile], model: &ReviewModel, node: usize) -> String {
        files[model.nodes[node].file]
            .path
            .rsplit('/')
            .next()
            .unwrap_or_default()
            .to_string()
    }

    /// `file:declaration,declaration` for each node.
    fn node_label(files: &[ChangedFile], model: &ReviewModel, node: usize) -> String {
        format!(
            "{}:{}",
            file_name(files, model, node),
            model.nodes[node].declarations.join(",")
        )
    }

    fn sub_column_labels(
        files: &[ChangedFile],
        model: &ReviewModel,
        title: &str,
    ) -> Vec<Vec<String>> {
        let frame = model.frame_with_title(title).expect("the frame exists");
        model.frames[frame]
            .sub_columns
            .iter()
            .map(|column| {
                let mut labels: Vec<String> = column
                    .iter()
                    .map(|node| node_label(files, model, *node))
                    .collect();
                labels.sort();
                labels
            })
            .collect()
    }

    fn edge_labels(files: &[ChangedFile], model: &ReviewModel) -> BTreeSet<(String, String)> {
        let label = |endpoint: Endpoint| match endpoint {
            Endpoint::Node(node) => node_label(files, model, node),
            Endpoint::Frame(frame) => model.frames[frame].title.to_string(),
        };
        model
            .edges
            .iter()
            .map(|edge| (label(edge.user), label(edge.dependency)))
            .collect()
    }

    #[test]
    fn test_symbol_model_of_the_discounts_branch() {
        let (files, model) = build(ReviewOrder::BottomUp, true);

        assert_eq!(
            sub_column_labels(&files, &model, "internal/orders"),
            [
                vec!["discount.go:Code", "discount_table_gen.go:"],
                vec!["discount_table_gen.go:discountTable", "orders.go:Order"],
                vec!["discount.go:percentFor"],
                vec!["discount.go:applyCode"],
                vec!["orders.go:Place"],
                vec!["orders_test.go:TestPlace,TestPlaceWithCode"],
            ]
        );
        let discount_nodes = (0..model.nodes.len())
            .filter(|node| file_name(&files, &model, *node) == "discount.go")
            .count();
        assert!(
            discount_nodes > 1,
            "a new file splits into its declarations"
        );

        let edges = edge_labels(&files, &model);
        let has_edge = |user: &str, dependency: &str| {
            edges.iter().any(|(edge_user, edge_dependency)| {
                edge_user.contains(user) && edge_dependency.contains(dependency)
            })
        };
        for (user, dependency) in [
            ("applyCode", "Discount"),
            ("applyCode", "percentFor"),
            ("percentFor", "discountTable"),
            ("Place", "applyCode"),
            ("Place", "Product"),
            ("ServeHTTP", "Place"),
            ("ServeHTTP", "ChargeWithRetry"),
            ("TestUnknownCode", "Place"),
        ] {
            assert!(has_edge(user, dependency), "missing {user} -> {dependency}");
        }
        for edge in &model.edges {
            assert!(matches!(edge.from, Endpoint::Node(_)) && matches!(edge.to, Endpoint::Node(_)));
        }

        let other_files: Vec<&str> = model
            .other_files
            .iter()
            .map(|node| files[model.nodes[*node].file].path.as_str())
            .collect();
        assert_eq!(
            other_files,
            [
                "README.md",
                "config/shop.yaml",
                "migrations/0002_discounts.sql"
            ]
        );

        let mut review_order = model.review_order();
        review_order.sort();
        review_order.dedup();
        assert_eq!(
            review_order.len(),
            model.nodes.len(),
            "every node is in the review order once"
        );
    }

    #[test]
    fn test_reversed_order_keeps_the_nodes_and_mirrors_columns() {
        let (files, bottom_up) = build(ReviewOrder::BottomUp, true);
        let (_, top_down) = build(ReviewOrder::TopDown, true);
        assert_eq!(bottom_up.nodes, top_down.nodes);

        let mut reversed = sub_column_labels(&files, &top_down, "internal/orders");
        reversed.reverse();
        assert_eq!(
            reversed,
            sub_column_labels(&files, &bottom_up, "internal/orders")
        );

        let column_of = |model: &ReviewModel, title: &str| {
            model
                .frame_with_title(title)
                .map(|frame| model.frames[frame].column)
        };
        assert_eq!(column_of(&bottom_up, "api"), Some(3));
        assert_eq!(column_of(&top_down, "api"), Some(0));
        assert_eq!(
            edge_labels(&files, &bottom_up),
            edge_labels(&files, &top_down)
        );
    }

    #[test]
    fn test_arrows_go_left_to_right() {
        for order in [ReviewOrder::BottomUp, ReviewOrder::TopDown] {
            let (_, model) = build(order, true);
            let body_heights = vec![px(100.); model.nodes.len()];
            let layout = Layout::new(&model, &body_heights);
            for edge in &model.edges {
                let from = layout.endpoint_bounds(edge.from).expect("the start exists");
                let to = layout.endpoint_bounds(edge.to).expect("the end exists");
                assert!(from.right() < to.left(), "{order:?}: {edge:?}");
            }
        }
    }

    #[test]
    fn test_layout_keeps_nodes_inside_their_frames() {
        let (_, model) = build(ReviewOrder::BottomUp, true);
        let body_heights = vec![px(100.); model.nodes.len()];
        let layout = Layout::new(&model, &body_heights);

        for (frame, bounds) in model.frames.iter().zip(&layout.frame_bounds) {
            for node in frame.nodes() {
                let node_bounds = layout.node_bounds[node];
                assert!(
                    bounds.contains(&node_bounds.origin)
                        && bounds.contains(&node_bounds.bottom_right()),
                    "a node must be inside its frame"
                );
            }
        }
        for (index, left) in layout.frame_bounds.iter().enumerate() {
            for right in layout.frame_bounds.iter().skip(index + 1) {
                assert!(!left.intersects(right), "frames must not overlap");
            }
        }

        let other_files = layout.other_files_bounds.expect("there are other files");
        let lowest_frame = layout
            .frame_bounds
            .iter()
            .map(|bounds| bounds.bottom())
            .fold(px(0.), |lowest, bottom| lowest.max(bottom));
        assert!(other_files.top() > lowest_frame);
    }

    #[test]
    fn test_without_symbols_files_are_nodes_and_frames_are_connected() {
        let (files, model) = build(ReviewOrder::BottomUp, false);
        let orders = model
            .frame_with_title("internal/orders")
            .expect("the frame exists");
        let labels: Vec<String> = model.frames[orders]
            .nodes()
            .map(|node| file_name(&files, &model, node))
            .collect();
        assert_eq!(
            labels,
            [
                "discount.go",
                "orders.go",
                "orders_test.go",
                "discount_table_gen.go"
            ]
        );
        assert!(
            model
                .frames
                .iter()
                .all(|frame| frame.sub_columns.len() == 1)
        );

        let mut edges: Vec<(String, String)> = edge_labels(&files, &model).into_iter().collect();
        edges.sort();
        assert_eq!(
            edges,
            [
                ("api", "internal/notify"),
                ("api", "internal/orders"),
                ("api", "internal/payments"),
                ("internal/catalog", "internal/money"),
                ("internal/orders", "internal/catalog"),
                ("internal/orders_test", "internal/orders"),
                ("internal/payments", "internal/money"),
            ]
            .map(|(user, dependency)| (user.to_string(), dependency.to_string()))
        );
    }

    #[test]
    fn test_without_packages_each_directory_is_a_frame() {
        let files = discounts_files();
        let model = ReviewModel::build(
            &files,
            None,
            None,
            Path::new("/project"),
            ReviewOrder::BottomUp,
        );
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
    fn test_relations_of_an_active_node() {
        for order in [ReviewOrder::BottomUp, ReviewOrder::TopDown] {
            let (files, model) = build(order, true);
            let apply_code = (0..model.nodes.len())
                .find(|node| node_label(&files, &model, *node).ends_with(":applyCode"))
                .expect("applyCode has its own node");
            let relations = model.node_relations(Some(Active::Node(apply_code)));
            let with = |relation: Relation| -> Vec<String> {
                let mut labels: Vec<String> = (0..model.nodes.len())
                    .filter(|node| relations[*node] == relation)
                    .map(|node| node_label(&files, &model, node))
                    .collect();
                labels.sort();
                labels
            };
            assert_eq!(with(Relation::Active), ["discount.go:applyCode"]);
            assert!(
                with(Relation::Dependency)
                    .iter()
                    .any(|label| label.contains("Discount")),
                "{order:?}: {:?}",
                with(Relation::Dependency)
            );
            assert!(
                with(Relation::User)
                    .iter()
                    .any(|label| label.contains("Place")),
                "{order:?}: {:?}",
                with(Relation::User)
            );

            let frame_relations = model.frame_relations(Some(Active::Node(apply_code)));
            let frame = |title: &str| {
                model
                    .frame_with_title(title)
                    .map(|frame| frame_relations[frame])
            };
            assert_eq!(
                frame("internal/orders"),
                Some(Relation::Active),
                "{order:?}"
            );
            assert_eq!(
                frame("internal/money"),
                Some(Relation::Dependency),
                "{order:?}"
            );
        }
    }
}
