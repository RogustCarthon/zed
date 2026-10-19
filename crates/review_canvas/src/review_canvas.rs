mod package_graph;
mod review_model;

use std::{
    cell::{Cell, RefCell},
    ffi::OsString,
    ops::Range,
    path::Path,
    rc::Rc,
    sync::Arc,
};

use anyhow::{Context as _, Result, anyhow};
use editor::{
    Editor, EditorMode, HiddenUnstagedDiffHunkRenderer, MinimapVisibility, SizingBehavior,
    multibuffer_context_lines,
};
use futures::{FutureExt as _, StreamExt as _, stream};
use futures_lite::future::yield_now;
use git::{repository::RepoPath, status::FileStatus};
use gpui::{
    App, AppContext as _, AsyncWindowContext, Bounds, Context, DispatchPhase, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, HitboxBehavior, Hsla, KeyBinding, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, PinchEvent, Pixels, Point, Render,
    ScrollDelta, ScrollWheelEvent, SharedString, Subscription, Task, TextStyleRefinement,
    WeakEntity, Window, actions, canvas, fill, point, px, size,
};
use language::{Capability, OffsetRangeExt as _, language_settings::SoftWrap};
use multi_buffer::{MultiBuffer, MultiBufferRow, PathKey};
use package_graph::{GO_LIST_JSON_FIELDS, GoPackages};
use project::{
    Project,
    git_store::diff_buffer_list::{BranchDiffEvent, DiffBase, DiffBufferList, LoadedDiffBuffer},
};
use review_model::{
    FRAME_HEADER_HEIGHT, Layout, NODE_HEADER_HEIGHT, NodeInfo, Relation, ReviewModel, ReviewOrder,
};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use ui::{Button, Icon, IconName, LabelSize, prelude::*};
use util::ResultExt as _;
use workspace::{Item, Workspace};

actions!(
    review_canvas,
    [
        /// Opens the review canvas for the changes since the merge base with the default branch.
        OpenReviewCanvas,
        /// Zooms the canvas in by one step.
        ZoomIn,
        /// Zooms the canvas out by one step.
        ZoomOut,
        /// Resets the canvas zoom to 100%.
        ResetZoom,
        /// Zooms the canvas so that all nodes are visible.
        ZoomToFit,
        /// Changes between bottom-up order (dependencies first) and top-down order (users first).
        ReverseOrder,
    ]
);

const KEY_CONTEXT: &str = "ReviewCanvas";

const MAX_CONCURRENT_BUFFER_LOADS: usize = 16;

const HEADER_FONT_SIZE: Pixels = px(12.);
const FRAME_TITLE_FONT_SIZE: Pixels = px(15.);
const INITIAL_CAMERA_ORIGIN: Point<Pixels> = point(px(-40.), px(-40.));
const CULLING_MARGIN: Pixels = px(200.);
const SCROLL_LINE_HEIGHT: Pixels = px(40.);
const WHEEL_DISTANCE_PER_ZOOM_STEP: Pixels = px(40.);

const ZOOM_STEP_FACTOR: f32 = 1.1;
const MIN_ZOOM_STEP: i32 = -25;
const MAX_ZOOM_STEP: i32 = 8;
/// Below this on-screen font size text is unreadable, so nodes paint line bars instead of
/// editors. This also keeps far zoom levels cheap.
const LOW_DETAIL_FONT_SIZE: Pixels = px(6.);

pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-=", ZoomIn, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd--", ZoomOut, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-0", ResetZoom, Some(KEY_CONTEXT)),
        KeyBinding::new("cmd-9", ZoomToFit, Some(KEY_CONTEXT)),
    ]);

    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|workspace, _: &OpenReviewCanvas, window, cx| {
            ReviewCanvas::open(workspace, window, cx);
        });
    })
    .detach();
}

struct CanvasNode {
    info: NodeInfo,
    title: SharedString,
    editor: Entity<Editor>,
    line_lengths: Arc<[u32]>,
    estimated_body_height: Pixels,
}

#[derive(Debug, Clone, PartialEq)]
enum LoadState {
    Loading,
    Ready,
    Failed(SharedString),
}

/// Maps world coordinates to coordinates local to the canvas element.
#[derive(Clone, Copy)]
struct Camera {
    /// The world point at the top-left corner of the canvas.
    origin: Point<Pixels>,
    zoom_step: i32,
}

impl Camera {
    fn zoom(&self) -> f32 {
        ZOOM_STEP_FACTOR.powi(self.zoom_step)
    }

    fn to_local(&self, world: Point<Pixels>) -> Point<Pixels> {
        let zoom = self.zoom();
        point(
            (world.x - self.origin.x) * zoom,
            (world.y - self.origin.y) * zoom,
        )
    }

    fn to_world(&self, local: Point<Pixels>) -> Point<Pixels> {
        let zoom = self.zoom();
        point(
            self.origin.x + local.x / zoom,
            self.origin.y + local.y / zoom,
        )
    }

    fn to_local_bounds(&self, world: Bounds<Pixels>) -> Bounds<Pixels> {
        let zoom = self.zoom();
        Bounds::new(
            self.to_local(world.origin),
            size(world.size.width * zoom, world.size.height * zoom),
        )
    }
}

struct Arrow {
    start: Point<Pixels>,
    end: Point<Pixels>,
    color: Hsla,
    emphasized: bool,
}

struct DragState {
    start_position: Point<Pixels>,
    start_origin: Point<Pixels>,
}

pub struct ReviewCanvas {
    focus_handle: FocusHandle,
    project: Entity<Project>,
    base_ref: Option<SharedString>,
    repository_root: Option<Arc<Path>>,
    diff_list: Option<Entity<DiffBufferList>>,
    load_state: LoadState,
    nodes: Vec<CanvasNode>,
    order: ReviewOrder,
    model: ReviewModel,
    go_packages: Option<Rc<GoPackages>>,
    /// Shows when the package graph is not available, for example when `go list` fails.
    graph_warning: Option<SharedString>,
    /// The output that `go list` gives in tests. `None` runs the real command.
    #[cfg(test)]
    go_list_output_for_tests: Option<String>,
    /// Editor body heights in world units. Starts as an estimate, then node paint replaces it
    /// with the measured height, because an editor sized by content only knows its height
    /// after layout.
    body_heights: Rc<RefCell<Vec<Pixels>>>,
    /// Bounds of the canvas element in window coordinates, from the last prepaint.
    viewport: Rc<Cell<Bounds<Pixels>>>,
    camera: Camera,
    drag: Option<DragState>,
    wheel_zoom_accumulator: f32,
    pinch_zoom_accumulator: f32,
    painted_node_count: usize,
    /// The frame that the user selected with a click. A focused node editor wins over this.
    selected_frame: Option<SharedString>,
    /// The frame whose arrows the last render highlighted.
    active_frame: Option<usize>,
    _diff_list_subscription: Option<Subscription>,
    _open_task: Task<()>,
    _refresh_task: Task<()>,
}

impl ReviewCanvas {
    fn open(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
        let project = workspace.project().clone();
        let review_canvas = cx.new(|cx| ReviewCanvas::new(project, window, cx));
        workspace.add_item_to_active_pane(Box::new(review_canvas), None, true, window, cx);
    }

    fn new(project: Entity<Project>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let open_task = cx.spawn_in(window, async move |this, cx| {
            if let Err(error) = Self::start_loading(this.clone(), cx).await {
                this.update(cx, |this, cx| this.fail(error, cx)).log_err();
            }
        });

        Self {
            focus_handle: cx.focus_handle(),
            project,
            base_ref: None,
            repository_root: None,
            diff_list: None,
            load_state: LoadState::Loading,
            nodes: Vec::new(),
            order: ReviewOrder::default(),
            model: ReviewModel::default(),
            go_packages: None,
            graph_warning: None,
            #[cfg(test)]
            go_list_output_for_tests: Some(String::new()),
            body_heights: Rc::default(),
            viewport: Rc::default(),
            camera: Camera {
                origin: INITIAL_CAMERA_ORIGIN,
                zoom_step: 0,
            },
            drag: None,
            wheel_zoom_accumulator: 0.,
            pinch_zoom_accumulator: 0.,
            painted_node_count: 0,
            selected_frame: None,
            active_frame: None,
            _diff_list_subscription: None,
            _open_task: open_task,
            _refresh_task: Task::ready(()),
        }
    }

    async fn start_loading(this: WeakEntity<Self>, cx: &mut AsyncWindowContext) -> Result<()> {
        let (repository, default_branch) = this.update(cx, |this, cx| {
            let repository = this
                .project
                .read(cx)
                .git_store()
                .read(cx)
                .active_repository()
                .context("This project has no git repository")?;
            let default_branch =
                repository.update(cx, |repository, _| repository.default_branch(true));
            anyhow::Ok((repository, default_branch))
        })??;
        let base_ref = default_branch
            .await??
            .context("Could not determine the default branch")?;

        this.update_in(cx, |this, window, cx| {
            this.repository_root = Some(repository.read(cx).snapshot().work_directory_abs_path);
            let git_store = this.project.read(cx).git_store().clone();
            let diff_list = cx.new(|cx| {
                DiffBufferList::new(
                    DiffBase::Merge {
                        base_ref: base_ref.clone(),
                    },
                    git_store,
                    Some(repository),
                    cx,
                )
            });
            this._diff_list_subscription = Some(cx.subscribe_in(
                &diff_list,
                window,
                |this, _, event, window, cx| match event {
                    BranchDiffEvent::FileListChanged | BranchDiffEvent::DiffBaseChanged => {
                        this.spawn_refresh(window, cx)
                    }
                },
            ));
            this.base_ref = Some(base_ref);
            this.diff_list = Some(diff_list);
            cx.notify();
        })
    }

    fn fail(&mut self, error: anyhow::Error, cx: &mut Context<Self>) {
        self.load_state = LoadState::Failed(format!("{error:#}").into());
        cx.notify();
    }

    fn spawn_refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._refresh_task = cx.spawn_in(window, async move |this, cx| {
            if let Err(error) = Self::refresh(this.clone(), cx).await {
                this.update(cx, |this, cx| this.fail(error, cx)).log_err();
            }
        });
    }

    async fn refresh(this: WeakEntity<Self>, cx: &mut AsyncWindowContext) -> Result<()> {
        let diff_buffers = this.update(cx, |this, cx| {
            let diff_list = this.diff_list.clone()?;
            diff_list.update(cx, |diff_list, cx| {
                // Before the merge-base tree diff loads, the list is empty. That is not the
                // same as "no changes", so keep the loading state.
                diff_list.statuses_by_path()?;
                Some(diff_list.load_buffers(cx))
            })
        })?;
        let Some(mut diff_buffers) = diff_buffers else {
            return Ok(());
        };
        diff_buffers.sort_by(|left, right| left.repo_path.cmp(&right.repo_path));

        // Ordered, so that the grid order follows the path order.
        let mut loads = stream::iter(diff_buffers.into_iter().map(|diff_buffer| {
            let repo_path = diff_buffer.repo_path;
            let file_status = diff_buffer.file_status;
            diff_buffer
                .load
                .map(move |loaded| (repo_path, file_status, loaded))
        }))
        .buffered(MAX_CONCURRENT_BUFFER_LOADS);

        let mut nodes = Vec::new();
        while let Some((repo_path, file_status, loaded)) = loads.next().await {
            // Buffered loads can already be complete when polled. Without a yield, this loop
            // would never await and would starve the foreground thread.
            yield_now().await;
            let Some(loaded) = loaded.log_err() else {
                continue;
            };
            let node = this.update_in(cx, |this, window, cx| {
                this.build_node(&repo_path, file_status, loaded, window, cx)
            })?;
            nodes.extend(node);
        }

        let go_packages = if nodes.iter().any(|node| node.info.is_go()) {
            let go_list = this.update(cx, |this, cx| this.go_list(cx))?;
            Some(go_list.await)
        } else {
            None
        };

        this.update(cx, |this, cx| this.set_nodes(nodes, go_packages, cx))
    }

    fn go_list(&self, cx: &mut App) -> Task<Result<GoPackages>> {
        // Tests use fixed output, so that they never run a process.
        #[cfg(test)]
        if let Some(output) = &self.go_list_output_for_tests {
            return Task::ready(GoPackages::parse(output.as_bytes()));
        }

        let Some(repository_root) = self.repository_root.clone() else {
            return Task::ready(Err(anyhow!("The repository root is not known")));
        };
        // The environment of the project shell, so that `go` is found also when Zed does not
        // start from a terminal.
        let project_environment = self.project.read(cx).environment().clone();
        let environment = project_environment.update(cx, |environment, cx| {
            environment.directory_environment(repository_root.clone(), cx)
        });
        cx.background_spawn(async move {
            let environment = environment.await;
            // If the project environment is not available, Zed's own `PATH` is the fallback.
            let search_path = environment
                .as_ref()
                .and_then(|environment| environment.get("PATH"))
                .map(OsString::from)
                .or_else(|| std::env::var_os("PATH"));
            let go = which::which_in("go", search_path, &repository_root)
                .context("Could not find the `go` command")?;
            let mut command = util::command::new_command(go);
            command
                .arg("list")
                .arg("-e")
                .arg(format!("-json={GO_LIST_JSON_FIELDS}"))
                .arg("./...")
                .current_dir(&repository_root);
            if let Some(environment) = environment {
                command.envs(environment);
            }
            let output = command.output().await.context("Could not run `go list`")?;
            anyhow::ensure!(
                output.status.success(),
                "`go list` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            GoPackages::parse(&output.stdout)
        })
    }

    fn build_node(
        &self,
        repo_path: &RepoPath,
        file_status: FileStatus,
        loaded: LoadedDiffBuffer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<CanvasNode> {
        let LoadedDiffBuffer {
            display_buffer,
            diff,
            ..
        } = loaded;

        let path = repo_path.as_unix_str().to_string();
        let info = NodeInfo {
            is_generated: path.ends_with(".go")
                && is_generated_go_file(&display_buffer.read(cx).text()),
            abs_path: self
                .repository_root
                .as_deref()
                .map(|root| root.join(repo_path.as_std_path()))
                .unwrap_or_else(|| repo_path.as_std_path().to_path_buf()),
            path,
        };

        let change_block_ranges: Vec<Range<language::Point>> = {
            let snapshot = display_buffer.read(cx).snapshot();
            let diff_snapshot = diff.read(cx).snapshot(cx);
            diff_snapshot
                .hunks(&snapshot)
                .map(|hunk| hunk.buffer_range.to_point(&snapshot))
                .collect()
        };
        // A status change without text change blocks (for example, a mode change) has
        // nothing to review on the canvas.
        if change_block_ranges.is_empty() {
            return None;
        }
        let change_block_count = change_block_ranges.len();

        let context_lines = multibuffer_context_lines(cx);
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadWrite);
            multibuffer.set_all_diff_hunks_expanded(cx);
            let path_key = PathKey::for_buffer(&display_buffer, cx);
            multibuffer.set_excerpts_for_path(
                path_key,
                display_buffer.clone(),
                change_block_ranges,
                context_lines,
                cx,
            );
            multibuffer.add_diff(diff, cx);
            multibuffer
        });

        let line_lengths: Arc<[u32]> = {
            let snapshot = multibuffer.read(cx).snapshot(cx);
            (0..=snapshot.max_point().row)
                .map(|row| snapshot.line_len(MultiBufferRow(row)))
                .collect()
        };

        let font_size = self.screen_font_size(cx);
        let editor = cx.new(|cx| {
            let mut editor = Editor::new(
                EditorMode::Full {
                    scale_ui_elements_with_buffer_font_size: true,
                    show_active_line_background: false,
                    sizing_behavior: SizingBehavior::SizeByContent,
                },
                multibuffer,
                Some(self.project.clone()),
                window,
                cx,
            );
            editor.set_soft_wrap_mode(SoftWrap::None, cx);
            editor.set_show_vertical_scrollbar(false, cx);
            editor.set_minimap_visibility(MinimapVisibility::Disabled, window, cx);
            editor.set_forbid_vertical_scroll(true);
            editor.set_expand_all_diff_hunks(cx);
            // Without a hunk renderer, the editor attaches the project's display diff (against
            // HEAD) to the buffer, and that replaces the merge-base diff of the node.
            editor.set_diff_hunk_renderer(Some(Arc::new(HiddenUnstagedDiffHunkRenderer)), cx);
            editor.set_text_style_refinement(font_size_refinement(font_size));
            editor
        });

        let settings = ThemeSettings::get_global(cx);
        let line_height = settings.buffer_font_size(cx) * settings.buffer_line_height.value();
        let estimated_rows = line_lengths.len() + change_block_count * 2;

        let status = if file_status.is_created() {
            "added"
        } else if file_status.is_deleted() {
            "deleted"
        } else {
            "modified"
        };
        let change_blocks = if change_block_count == 1 {
            "1 change".to_string()
        } else {
            format!("{change_block_count} changes")
        };

        let generated = if info.is_generated {
            " · generated"
        } else {
            ""
        };
        Some(CanvasNode {
            title: format!("{} · {status} · {change_blocks}{generated}", info.path).into(),
            info,
            editor,
            line_lengths,
            estimated_body_height: line_height * estimated_rows as f32,
        })
    }

    fn set_nodes(
        &mut self,
        nodes: Vec<CanvasNode>,
        go_packages: Option<Result<GoPackages>>,
        cx: &mut Context<Self>,
    ) {
        *self.body_heights.borrow_mut() = nodes
            .iter()
            .map(|node| node.estimated_body_height)
            .collect();
        self.nodes = nodes;
        (self.go_packages, self.graph_warning) = match go_packages {
            Some(Ok(go_packages)) => (Some(Rc::new(go_packages)), None),
            Some(Err(error)) => (None, Some(format!("No package graph: {error:#}").into())),
            None => (None, None),
        };
        self.rebuild_model();
        self.load_state = LoadState::Ready;
        // The zoom can change while the nodes load.
        self.apply_zoom_to_editors(cx);
        cx.notify();
    }

    fn rebuild_model(&mut self) {
        let infos: Vec<NodeInfo> = self.nodes.iter().map(|node| node.info.clone()).collect();
        let repository_root = self.repository_root.as_deref().unwrap_or(Path::new(""));
        self.model = ReviewModel::build(
            &infos,
            self.go_packages.as_deref(),
            repository_root,
            self.order,
        );
    }

    fn reverse_order(&mut self, cx: &mut Context<Self>) {
        self.order = self.order.reversed();
        self.rebuild_model();
        // Show the start of the new review order.
        self.camera.origin = INITIAL_CAMERA_ORIGIN;
        cx.notify();
    }

    fn base_font_size(&self, cx: &App) -> Pixels {
        ThemeSettings::get_global(cx).buffer_font_size(cx)
    }

    fn screen_font_size(&self, cx: &App) -> Pixels {
        self.base_font_size(cx) * self.camera.zoom()
    }

    fn apply_zoom_to_editors(&self, cx: &mut Context<Self>) {
        let font_size = self.screen_font_size(cx);
        for node in &self.nodes {
            node.editor.update(cx, |editor, cx| {
                editor.set_text_style_refinement(font_size_refinement(font_size));
                cx.notify();
            });
        }
    }

    fn layout(&self) -> Layout {
        Layout::new(&self.model, &self.body_heights.borrow())
    }

    fn set_zoom_step(
        &mut self,
        zoom_step: i32,
        anchor: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let zoom_step = zoom_step.clamp(MIN_ZOOM_STEP, MAX_ZOOM_STEP);
        if zoom_step == self.camera.zoom_step {
            return;
        }
        let anchor = anchor.unwrap_or_else(|| {
            let viewport = self.viewport.get();
            point(viewport.size.width / 2., viewport.size.height / 2.)
        });
        let world_anchor = self.camera.to_world(anchor);
        self.camera.zoom_step = zoom_step;
        let zoom = self.camera.zoom();
        self.camera.origin = point(
            world_anchor.x - anchor.x / zoom,
            world_anchor.y - anchor.y / zoom,
        );
        self.apply_zoom_to_editors(cx);
        cx.notify();
    }

    fn zoom_by_steps(&mut self, steps: i32, anchor: Option<Point<Pixels>>, cx: &mut Context<Self>) {
        self.set_zoom_step(self.camera.zoom_step + steps, anchor, cx);
    }

    fn zoom_to_fit(&mut self, cx: &mut Context<Self>) {
        let Some(content) = self.layout().content_bounds() else {
            return;
        };
        let viewport = self.viewport.get();
        let fit_zoom = (viewport.size.width / content.size.width)
            .min(viewport.size.height / content.size.height)
            * 0.95;
        let zoom_step = (fit_zoom.ln() / ZOOM_STEP_FACTOR.ln()).floor() as i32;
        self.set_zoom_step(zoom_step, None, cx);
        let zoom = self.camera.zoom();
        let center = content.center();
        self.camera.origin = point(
            center.x - viewport.size.width / 2. / zoom,
            center.y - viewport.size.height / 2. / zoom,
        );
        cx.notify();
    }

    fn handle_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        local_position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let delta = match event.delta {
            ScrollDelta::Pixels(delta) => delta,
            ScrollDelta::Lines(lines) => {
                point(SCROLL_LINE_HEIGHT * lines.x, SCROLL_LINE_HEIGHT * lines.y)
            }
        };

        if event.modifiers.platform {
            self.wheel_zoom_accumulator += delta.y / WHEEL_DISTANCE_PER_ZOOM_STEP;
            let steps = self.wheel_zoom_accumulator.trunc();
            if steps != 0. {
                self.wheel_zoom_accumulator -= steps;
                self.zoom_by_steps(steps as i32, Some(local_position), cx);
            }
        } else {
            let zoom = self.camera.zoom();
            self.camera.origin = point(
                self.camera.origin.x - delta.x / zoom,
                self.camera.origin.y - delta.y / zoom,
            );
            cx.notify();
        }
    }

    fn handle_pinch(
        &mut self,
        event: &PinchEvent,
        local_position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        // Accumulate in log space so that pinch deltas compose multiplicatively with zoom steps.
        self.pinch_zoom_accumulator += (1. + event.delta).max(0.01).ln() / ZOOM_STEP_FACTOR.ln();
        let steps = self.pinch_zoom_accumulator.trunc();
        if steps != 0. {
            self.pinch_zoom_accumulator -= steps;
            self.zoom_by_steps(steps as i32, Some(local_position), cx);
        }
    }

    /// Paints the arrows that do not touch the active frame, below the frames, and owns the
    /// mouse input of the canvas.
    fn render_background_layer(
        &self,
        arrows: Vec<Arrow>,
        node_targets: Vec<(Bounds<Pixels>, Option<SharedString>)>,
        frame_targets: Vec<(Bounds<Pixels>, SharedString)>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let this = cx.weak_entity();
        let viewport = self.viewport.clone();
        let focus_handle = self.focus_handle.clone();
        let zoom = self.camera.zoom();

        canvas(
            {
                let this = this.clone();
                move |bounds, window, cx| {
                    if viewport.get() != bounds {
                        viewport.set(bounds);
                        // The first frame renders before the canvas knows its size.
                        cx.defer(move |cx| {
                            this.update(cx, |_, cx| cx.notify()).log_err();
                        });
                    }
                    window.insert_hitbox(bounds, HitboxBehavior::Normal)
                }
            },
            move |bounds, hitbox, window, _cx| {
                for arrow in &arrows {
                    paint_arrow(bounds.origin, arrow, zoom, window);
                }

                // The canvas owns scrolling and pinching. It listens in the capture phase so
                // that node editors never scroll, and all wheel input pans the camera.
                window.on_mouse_event({
                    let this = this.clone();
                    let hitbox = hitbox.clone();
                    move |event: &ScrollWheelEvent, phase, window, cx| {
                        if phase != DispatchPhase::Capture || !hitbox.should_handle_scroll(window) {
                            return;
                        }
                        let local_position = event.position - bounds.origin;
                        this.update(cx, |this, cx| {
                            this.handle_scroll_wheel(event, local_position, cx)
                        })
                        .log_err();
                        cx.stop_propagation();
                    }
                });

                window.on_mouse_event({
                    let this = this.clone();
                    let hitbox = hitbox.clone();
                    move |event: &PinchEvent, phase, window, cx| {
                        if phase != DispatchPhase::Capture || !hitbox.is_hovered(window) {
                            return;
                        }
                        let local_position = event.position - bounds.origin;
                        this.update(cx, |this, cx| this.handle_pinch(event, local_position, cx))
                            .log_err();
                        cx.stop_propagation();
                    }
                });

                window.on_mouse_event({
                    let this = this.clone();
                    move |event: &MouseDownEvent, phase, window, cx| {
                        if phase != DispatchPhase::Bubble
                            || event.button != MouseButton::Left
                            || !hitbox.is_hovered(window)
                        {
                            return;
                        }
                        let local_position = event.position - bounds.origin;
                        if let Some((_, frame)) = node_targets
                            .iter()
                            .find(|(node_bounds, _)| node_bounds.contains(&local_position))
                        {
                            // The node editor handles the click. Its frame becomes the
                            // selection, so that it stays active after the editor loses focus.
                            let frame = frame.clone();
                            this.update(cx, |this, cx| {
                                this.selected_frame = frame;
                                cx.notify();
                            })
                            .log_err();
                            return;
                        }
                        let frame = frame_targets
                            .iter()
                            .find(|(frame_bounds, _)| frame_bounds.contains(&local_position))
                            .map(|(_, title)| title.clone());
                        focus_handle.focus(window, cx);
                        this.update(cx, |this, cx| {
                            this.selected_frame = frame;
                            this.drag = Some(DragState {
                                start_position: event.position,
                                start_origin: this.camera.origin,
                            });
                            cx.notify();
                        })
                        .log_err();
                    }
                });

                window.on_mouse_event({
                    let this = this.clone();
                    move |event: &MouseMoveEvent, phase, _window, cx| {
                        if phase != DispatchPhase::Bubble {
                            return;
                        }
                        this.update(cx, |this, cx| {
                            let Some(drag) = &this.drag else {
                                return;
                            };
                            if event.pressed_button != Some(MouseButton::Left) {
                                this.drag = None;
                                return;
                            }
                            let zoom = this.camera.zoom();
                            this.camera.origin = point(
                                drag.start_origin.x
                                    - (event.position.x - drag.start_position.x) / zoom,
                                drag.start_origin.y
                                    - (event.position.y - drag.start_position.y) / zoom,
                            );
                            cx.notify();
                        })
                        .log_err();
                    }
                });

                window.on_mouse_event(move |_: &MouseUpEvent, phase, _window, cx| {
                    if phase != DispatchPhase::Bubble {
                        return;
                    }
                    this.update(cx, |this, _| this.drag = None).log_err();
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    fn render_node(
        &self,
        index: usize,
        local_bounds: Bounds<Pixels>,
        low_detail: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let node = self.nodes.get(index)?;
        let zoom = self.camera.zoom();
        let colors = cx.theme().colors();
        let header_height = NODE_HEADER_HEIGHT * zoom;

        let body = if low_detail {
            let line_lengths = node.line_lengths.clone();
            let bar_color = colors.text_muted.opacity(0.35);
            let body_height = local_bounds.size.height - header_height;
            div()
                .w_full()
                .h(body_height)
                .child(
                    canvas(
                        |_, _, _| {},
                        move |bounds, _, window, _| {
                            paint_line_bars(bounds, &line_lengths, bar_color, window)
                        },
                    )
                    .size_full(),
                )
                .into_any_element()
        } else {
            let body_heights = self.body_heights.clone();
            let this = cx.weak_entity();
            div()
                .relative()
                .w_full()
                .child(node.editor.clone())
                .child(
                    canvas(
                        move |bounds, _window, cx| {
                            let world_height = bounds.size.height / zoom;
                            let mut body_heights = body_heights.borrow_mut();
                            if let Some(stored_height) = body_heights.get_mut(index)
                                && (*stored_height - world_height).abs() > px(0.5)
                            {
                                *stored_height = world_height;
                                cx.defer(move |cx| {
                                    this.update(cx, |_, cx| cx.notify()).log_err();
                                });
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                )
                .into_any_element()
        };

        Some(
            div()
                .absolute()
                .left(local_bounds.origin.x)
                .top(local_bounds.origin.y)
                .w(local_bounds.size.width)
                .flex()
                .flex_col()
                .overflow_hidden()
                .rounded(px(6.) * zoom)
                .border_1()
                .border_color(colors.border)
                .bg(colors.editor_background)
                .child(
                    div()
                        .h(header_height)
                        .flex_none()
                        .flex()
                        .items_center()
                        .px(px(10.) * zoom)
                        .border_b_1()
                        .border_color(colors.border_variant)
                        .bg(colors.tab_bar_background)
                        .text_size(HEADER_FONT_SIZE * zoom)
                        .text_color(colors.text_muted)
                        .whitespace_nowrap()
                        .child(node.title.clone()),
                )
                .child(body)
                .into_any_element(),
        )
    }

    fn render_status(&self, low_detail: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = cx.theme().colors();
        let detail = if low_detail { " · low detail" } else { "" };
        let order = match self.order {
            ReviewOrder::BottomUp => "bottom-up",
            ReviewOrder::TopDown => "top-down",
        };
        div()
            .absolute()
            .bottom_2()
            .right_2()
            .max_w(px(640.))
            .flex()
            .flex_col()
            .items_end()
            .gap_1()
            .px_2()
            .py_1()
            .rounded_md()
            .border_1()
            .border_color(colors.border)
            .bg(colors.elevated_surface_background)
            .text_xs()
            .text_color(colors.text_muted)
            // Clicks on the status bar must not start a drag of the canvas below it.
            .occlude()
            .when_some(self.graph_warning.clone(), |status, warning| {
                status.child(div().text_color(colors.text).child(warning))
            })
            .when_some(self.render_active_frame_legend(cx), |status, legend| {
                status.child(legend)
            })
            .child(
                h_flex()
                    .gap_2()
                    .child(format!(
                        "zoom {:.0}% · {} of {} nodes painted · {order}{detail}",
                        self.camera.zoom() * 100.,
                        self.painted_node_count,
                        self.nodes.len(),
                    ))
                    .child(
                        Button::new("reverse-order", "Reverse order")
                            .label_size(LabelSize::XSmall)
                            .on_click(cx.listener(|this, _, _, cx| this.reverse_order(cx))),
                    ),
            )
    }

    fn render_active_frame_legend(&self, cx: &App) -> Option<impl IntoElement> {
        let active = self.active_frame?;
        let frame = self.model.frames.get(active)?;
        let edge_relations = self.model.edge_relations(Some(active));
        let count = |relation: Relation| {
            edge_relations
                .iter()
                .filter(|edge_relation| **edge_relation == relation)
                .count()
        };
        let entry = |relation: Relation, label: &str| {
            div()
                .text_color(Self::relation_color(relation, cx).unwrap_or_default())
                .child(format!("● {label} {}", count(relation)))
        };
        Some(
            h_flex()
                .gap_2()
                .child(
                    div()
                        .text_color(cx.theme().colors().text)
                        .child(frame.title.clone()),
                )
                .child(entry(Relation::Dependency, "uses"))
                .child(entry(Relation::User, "used by")),
        )
    }

    fn render_message(&self, message: SharedString, cx: &mut Context<Self>) -> AnyElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(cx.theme().colors().text_muted)
            .child(message)
            .into_any_element()
    }

    fn compute_active_frame(&self, window: &Window, cx: &App) -> Option<usize> {
        let focused_node = self
            .nodes
            .iter()
            .position(|node| node.editor.focus_handle(cx).contains_focused(window, cx));
        match focused_node {
            Some(node) => self.model.frame_of_node(node),
            None => self
                .selected_frame
                .as_deref()
                .and_then(|title| self.model.frame_with_title(title)),
        }
    }

    fn relation_color(relation: Relation, cx: &App) -> Option<Hsla> {
        match relation {
            Relation::Dependency => Some(cx.theme().status().info),
            Relation::User => Some(cx.theme().status().warning),
            Relation::Active => Some(cx.theme().colors().border_focused),
            Relation::Unrelated => None,
        }
    }

    fn render_canvas(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let zoom = self.camera.zoom();
        let viewport_size = self.viewport.get().size;
        let culling_bounds = Bounds::new(Point::default(), viewport_size).dilate(CULLING_MARGIN);
        let low_detail = self.screen_font_size(cx) < LOW_DETAIL_FONT_SIZE;
        self.active_frame = self.compute_active_frame(window, cx);
        let frame_relations = self.model.frame_relations(self.active_frame);
        let edge_relations = self.model.edge_relations(self.active_frame);

        let layout = self.layout();
        let to_local = |bounds: &Bounds<Pixels>| self.camera.to_local_bounds(*bounds);
        let node_bounds: Vec<Bounds<Pixels>> = layout.node_bounds.iter().map(to_local).collect();
        let frame_bounds: Vec<Bounds<Pixels>> = layout.frame_bounds.iter().map(to_local).collect();
        let other_files_bounds = layout.other_files_bounds.as_ref().map(to_local);

        let visible_nodes: Vec<usize> = node_bounds
            .iter()
            .enumerate()
            .filter(|(_, bounds)| bounds.intersects(&culling_bounds))
            .map(|(index, _)| index)
            .collect();
        self.painted_node_count = visible_nodes.len();

        // With an active frame, the other arrows fade, so that the highlighted ones stand out.
        let unrelated_arrow_color =
            cx.theme()
                .colors()
                .text_muted
                .opacity(if self.active_frame.is_some() {
                    0.2
                } else {
                    0.7
                });
        let header_middle = FRAME_HEADER_HEIGHT * zoom / 2.;
        let (highlighted_arrows, other_arrows): (Vec<Arrow>, Vec<Arrow>) = self
            .model
            .edges
            .iter()
            .zip(&edge_relations)
            .filter_map(|(edge, relation)| {
                let from = frame_bounds.get(edge.from)?;
                let to = frame_bounds.get(edge.to)?;
                let start = point(from.right(), from.top() + header_middle);
                let end = point(to.left(), to.top() + header_middle);
                let span = Bounds::from_corners(
                    point(start.x.min(end.x), start.y.min(end.y)),
                    point(start.x.max(end.x), start.y.max(end.y)),
                );
                if !span.intersects(&culling_bounds) {
                    return None;
                }
                let color = Self::relation_color(*relation, cx);
                Some(Arrow {
                    start,
                    end,
                    color: color.unwrap_or(unrelated_arrow_color),
                    emphasized: color.is_some(),
                })
            })
            .partition(|arrow| arrow.emphasized);

        let node_targets: Vec<(Bounds<Pixels>, Option<SharedString>)> = visible_nodes
            .iter()
            .filter_map(|index| {
                let frame = self
                    .model
                    .frame_of_node(*index)
                    .and_then(|frame| self.model.frames.get(frame))
                    .map(|frame| frame.title.clone());
                Some((*node_bounds.get(*index)?, frame))
            })
            .collect();
        let frame_targets: Vec<(Bounds<Pixels>, SharedString)> = self
            .model
            .frames
            .iter()
            .zip(&frame_bounds)
            .map(|(frame, bounds)| (*bounds, frame.title.clone()))
            .collect();

        let frames: Vec<AnyElement> = self
            .model
            .frames
            .iter()
            .zip(&frame_bounds)
            .zip(&frame_relations)
            .map(|((frame, bounds), relation)| (frame.title.clone(), *bounds, *relation))
            .chain(
                other_files_bounds
                    .map(|bounds| ("Other files".into(), bounds, Relation::Unrelated)),
            )
            .filter(|(_, bounds, _)| bounds.intersects(&culling_bounds))
            .map(|(title, bounds, relation)| self.render_frame(title, bounds, relation, cx))
            .collect();

        let nodes: Vec<AnyElement> = visible_nodes
            .iter()
            .filter_map(|index| {
                let bounds = *node_bounds.get(*index)?;
                self.render_node(*index, bounds, low_detail, cx)
            })
            .collect();

        div()
            .relative()
            .size_full()
            .overflow_hidden()
            .child(self.render_background_layer(other_arrows, node_targets, frame_targets, cx))
            .children(frames)
            // Highlighted arrows paint above the frames, so that an arrow that crosses a column
            // stays visible. Nodes still paint above them.
            .child(
                canvas(
                    |_, _, _| {},
                    move |bounds, _, window, _| {
                        for arrow in &highlighted_arrows {
                            paint_arrow(bounds.origin, arrow, zoom, window);
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
            .children(nodes)
            .child(self.render_status(low_detail, cx))
            .into_any_element()
    }

    fn render_frame(
        &self,
        title: SharedString,
        local_bounds: Bounds<Pixels>,
        relation: Relation,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let zoom = self.camera.zoom();
        let colors = cx.theme().colors();
        let border_color = Self::relation_color(relation, cx).unwrap_or(colors.border_variant);
        div()
            .absolute()
            .left(local_bounds.origin.x)
            .top(local_bounds.origin.y)
            .w(local_bounds.size.width)
            .h(local_bounds.size.height)
            .rounded(px(10.) * zoom)
            .map(|frame| match relation {
                Relation::Unrelated => frame.border_1(),
                Relation::Active | Relation::Dependency | Relation::User => frame.border_2(),
            })
            .border_color(border_color)
            .bg(colors.surface_background)
            .child(
                div()
                    .h(FRAME_HEADER_HEIGHT * zoom)
                    .flex()
                    .items_center()
                    .px(px(16.) * zoom)
                    .text_size(FRAME_TITLE_FONT_SIZE * zoom)
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(colors.text)
                    .whitespace_nowrap()
                    .child(title),
            )
            .into_any_element()
    }
}

impl Render for ReviewCanvas {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.painted_node_count = 0;
        self.active_frame = None;
        let content = match &self.load_state {
            LoadState::Loading => self.render_message("Loading changes…".into(), cx),
            LoadState::Failed(message) => self.render_message(message.clone(), cx),
            LoadState::Ready if self.nodes.is_empty() => {
                let message = match &self.base_ref {
                    Some(base_ref) => format!("No changes since {base_ref}").into(),
                    None => "No changes".into(),
                };
                self.render_message(message, cx)
            }
            LoadState::Ready => self.render_canvas(window, cx),
        };

        div()
            .id("review-canvas")
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom_by_steps(1, None, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom_by_steps(-1, None, cx)))
            .on_action(cx.listener(|this, _: &ResetZoom, _, cx| this.set_zoom_step(0, None, cx)))
            .on_action(cx.listener(|this, _: &ZoomToFit, _, cx| this.zoom_to_fit(cx)))
            .on_action(cx.listener(|this, _: &ReverseOrder, _, cx| this.reverse_order(cx)))
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(content)
    }
}

impl EventEmitter<()> for ReviewCanvas {}

impl Focusable for ReviewCanvas {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for ReviewCanvas {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        match &self.base_ref {
            Some(base_ref) => format!("Review since {base_ref}").into(),
            None => "Review".into(),
        }
    }

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }
}

/// Go marks generated files with a comment before the package clause:
/// `// Code generated ... DO NOT EDIT.`
fn is_generated_go_file(text: &str) -> bool {
    text.lines()
        .take_while(|line| !line.starts_with("package "))
        .any(|line| line.starts_with("// Code generated ") && line.ends_with(" DO NOT EDIT."))
}

fn font_size_refinement(font_size: Pixels) -> TextStyleRefinement {
    TextStyleRefinement {
        font_size: Some(font_size.into()),
        ..Default::default()
    }
}

fn paint_arrow(origin: Point<Pixels>, arrow: &Arrow, zoom: f32, window: &mut Window) {
    let start = origin + arrow.start;
    let end = origin + arrow.end;
    let base_width = if arrow.emphasized { px(3.) } else { px(1.5) };
    let stroke_width = (base_width * zoom).max(px(1.));
    let control_offset = (end.x - start.x).abs() / 2.;
    let mut curve = PathBuilder::stroke(stroke_width);
    curve.move_to(start);
    curve.cubic_bezier_to(
        end,
        point(start.x + control_offset, start.y),
        point(end.x - control_offset, end.y),
    );
    if let Some(path) = curve.build().log_err() {
        window.paint_path(path, arrow.color);
    }

    let base_head_size = if arrow.emphasized { px(13.) } else { px(9.) };
    let head_size = (base_head_size * zoom).max(px(4.));
    let mut head = PathBuilder::fill();
    head.move_to(end);
    head.line_to(point(end.x - head_size, end.y - head_size / 2.));
    head.line_to(point(end.x - head_size, end.y + head_size / 2.));
    head.close();
    if let Some(path) = head.build().log_err() {
        window.paint_path(path, arrow.color);
    }
}

fn paint_line_bars(bounds: Bounds<Pixels>, line_lengths: &[u32], color: Hsla, window: &mut Window) {
    if line_lengths.is_empty() {
        return;
    }
    const FULL_WIDTH_COLUMNS: f32 = 100.;
    let line_height = bounds.size.height / line_lengths.len() as f32;
    let bar_height = (line_height * 0.6).max(px(1.));
    let left = bounds.origin.x + bounds.size.width * 0.04;
    let max_width = bounds.size.width * 0.92;
    for (row, line_length) in line_lengths.iter().enumerate() {
        if *line_length == 0 {
            continue;
        }
        let width = max_width * (*line_length as f32 / FULL_WIDTH_COLUMNS).min(1.);
        let top = bounds.origin.y + line_height * row as f32;
        window.paint_quad(fill(
            Bounds::new(point(left, top), size(width, bar_height)),
            color,
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext};
    use project::FakeFs;
    use serde_json::{Map, Value, json};
    use settings::SettingsStore;
    use std::path::Path;
    use util::path;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
    }

    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));
    }

    async fn open_review_canvas(
        tree: Value,
        cx: &mut TestAppContext,
    ) -> (Arc<FakeFs>, Entity<ReviewCanvas>, &mut VisualTestContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), tree).await;
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            ReviewCanvas::open(workspace, window, cx)
        });
        cx.run_until_parked();
        let review_canvas = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ReviewCanvas>(cx)
            })
            .expect("the review canvas is the active item");
        (fs, review_canvas, cx)
    }

    fn node_paths(review_canvas: &Entity<ReviewCanvas>, cx: &mut VisualTestContext) -> Vec<String> {
        review_canvas.read_with(cx, |review_canvas, _| {
            review_canvas
                .nodes
                .iter()
                .filter_map(|node| node.title.split(" · ").next().map(str::to_string))
                .collect()
        })
    }

    #[gpui::test]
    async fn test_nodes_are_the_changed_files(cx: &mut TestAppContext) {
        init_test(cx);
        let (fs, review_canvas, cx) = open_review_canvas(
            json!({
                ".git": {},
                "a.txt": "C",
                "b.txt": "new",
                "c.txt": "in-merge-base-and-work-tree",
                "d.txt": "created-in-head",
            }),
            cx,
        )
        .await;

        fs.set_head_for_repo(
            Path::new(path!("/project/.git")),
            &[("a.txt", "B".into()), ("d.txt", "created-in-head".into())],
            "sha",
        );
        fs.set_merge_base_content_for_repo(
            Path::new(path!("/project/.git")),
            &[
                ("a.txt", "A".into()),
                ("c.txt", "in-merge-base-and-work-tree".into()),
            ],
        );
        draw(cx);

        review_canvas.read_with(cx, |review_canvas, _| {
            assert_eq!(review_canvas.load_state, LoadState::Ready);
            assert_eq!(review_canvas.base_ref.as_deref(), Some("origin/main"));
        });
        assert_eq!(node_paths(&review_canvas, cx), ["a.txt", "b.txt", "d.txt"]);

        let first_editor =
            review_canvas.read_with(cx, |review_canvas, _| review_canvas.nodes[0].editor.clone());
        editor::test::editor_test_context::assert_state_with_diff(&first_editor, cx, "- ˇA\n+ C");
    }

    #[gpui::test]
    async fn test_nodes_diff_against_the_merge_base(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/project"),
            json!({
                ".git": {},
                "a.txt": "one\ntwo changed on the branch\nthree\n",
            }),
        )
        .await;
        // A committed branch with a clean working tree: HEAD equals the working tree, and only
        // the merge base differs.
        fs.set_head_for_repo(
            Path::new(path!("/project/.git")),
            &[("a.txt", "one\ntwo changed on the branch\nthree\n".into())],
            "sha",
        );
        fs.set_merge_base_content_for_repo(
            Path::new(path!("/project/.git")),
            &[("a.txt", "one\ntwo\nthree\n".into())],
        );
        let project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        cx.run_until_parked();
        workspace.update_in(cx, |workspace, window, cx| {
            ReviewCanvas::open(workspace, window, cx)
        });
        draw(cx);

        let review_canvas = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ReviewCanvas>(cx)
            })
            .expect("the review canvas is the active item");
        assert_eq!(node_paths(&review_canvas, cx), ["a.txt"]);
        let editor =
            review_canvas.read_with(cx, |review_canvas, _| review_canvas.nodes[0].editor.clone());
        editor::test::editor_test_context::assert_state_with_diff(
            &editor,
            cx,
            "  ˇone\n- two\n+ two changed on the branch\n  three\n",
        );
    }

    #[gpui::test]
    async fn test_no_changes_shows_no_nodes(cx: &mut TestAppContext) {
        init_test(cx);
        let (fs, review_canvas, cx) = open_review_canvas(
            json!({
                ".git": {},
                "a.txt": "same",
            }),
            cx,
        )
        .await;

        fs.set_head_for_repo(
            Path::new(path!("/project/.git")),
            &[("a.txt", "same".into())],
            "sha",
        );
        fs.set_merge_base_content_for_repo(
            Path::new(path!("/project/.git")),
            &[("a.txt", "same".into())],
        );
        draw(cx);

        review_canvas.read_with(cx, |review_canvas, _| {
            assert_eq!(review_canvas.load_state, LoadState::Ready);
        });
        assert_eq!(node_paths(&review_canvas, cx), Vec::<String>::new());
        review_canvas.read_with(cx, |review_canvas, _| {
            assert_eq!(review_canvas.painted_node_count, 0);
        });
    }

    #[gpui::test]
    async fn test_project_without_repository_shows_error(cx: &mut TestAppContext) {
        init_test(cx);
        let (_fs, review_canvas, cx) = open_review_canvas(json!({ "a.txt": "text" }), cx).await;
        draw(cx);

        review_canvas.read_with(cx, |review_canvas, _| {
            assert_eq!(
                review_canvas.load_state,
                LoadState::Failed("This project has no git repository".into())
            );
            assert!(review_canvas.nodes.is_empty());
        });
    }

    #[gpui::test]
    async fn test_frames_arrows_and_other_files(cx: &mut TestAppContext) {
        init_test(cx);
        let (fs, review_canvas, cx) = open_review_canvas(
            json!({
                ".git": {},
                "README.md": "new readme\n",
                "internal": {
                    "money": { "money.go": "package money\n\n// new\n" },
                    "catalog": { "catalog.go": "package catalog\n\n// new\n" },
                },
            }),
            cx,
        )
        .await;
        review_canvas.update(cx, |review_canvas, _| {
            review_canvas.go_list_output_for_tests =
                Some(include_str!("../test_data/shop_go_list.json").to_string());
        });

        let files = [
            ("README.md", "readme\n"),
            ("internal/money/money.go", "package money\n"),
            ("internal/catalog/catalog.go", "package catalog\n"),
        ];
        let base: Vec<(&str, String)> = files
            .iter()
            .map(|(path, text)| (*path, text.to_string()))
            .collect();
        fs.set_head_for_repo(Path::new(path!("/project/.git")), &base, "sha");
        fs.set_merge_base_content_for_repo(Path::new(path!("/project/.git")), &base);
        draw(cx);

        let summary = |review_canvas: &ReviewCanvas| {
            let frames: Vec<(String, usize)> = review_canvas
                .model
                .frames
                .iter()
                .map(|frame| (frame.title.to_string(), frame.column))
                .collect();
            let edges: Vec<(String, String)> = review_canvas
                .model
                .edges
                .iter()
                .map(|edge| {
                    (
                        review_canvas.model.frames[edge.from].title.to_string(),
                        review_canvas.model.frames[edge.to].title.to_string(),
                    )
                })
                .collect();
            let other_files: Vec<String> = review_canvas
                .model
                .other_files
                .iter()
                .map(|node| review_canvas.nodes[*node].info.path.clone())
                .collect();
            (frames, edges, other_files)
        };

        review_canvas.read_with(cx, |review_canvas, _| {
            assert_eq!(review_canvas.graph_warning, None);
            let (frames, edges, other_files) = summary(review_canvas);
            assert_eq!(
                frames,
                [
                    ("internal/money".to_string(), 0),
                    ("internal/catalog".to_string(), 1)
                ]
            );
            assert_eq!(
                edges,
                [("internal/money".to_string(), "internal/catalog".to_string())]
            );
            assert_eq!(other_files, ["README.md"]);
        });

        let catalog_editor = review_canvas.read_with(cx, |review_canvas, _| {
            review_canvas
                .nodes
                .iter()
                .find(|node| node.info.path == "internal/catalog/catalog.go")
                .map(|node| node.editor.clone())
                .expect("catalog.go has a node")
        });
        cx.update(|window, cx| catalog_editor.focus_handle(cx).focus(window, cx));
        draw(cx);
        let active_title = |review_canvas: &ReviewCanvas| {
            review_canvas
                .active_frame
                .and_then(|frame| review_canvas.model.frames.get(frame))
                .map(|frame| frame.title.to_string())
        };
        review_canvas.read_with(cx, |review_canvas, _| {
            assert_eq!(
                active_title(review_canvas).as_deref(),
                Some("internal/catalog")
            );
            assert_eq!(
                review_canvas
                    .model
                    .edge_relations(review_canvas.active_frame),
                [Relation::Dependency]
            );
        });

        cx.dispatch_action(ReverseOrder);
        draw(cx);
        review_canvas.read_with(cx, |review_canvas, _| {
            assert_eq!(
                active_title(review_canvas).as_deref(),
                Some("internal/catalog")
            );
            assert_eq!(
                review_canvas
                    .model
                    .edge_relations(review_canvas.active_frame),
                [Relation::Dependency],
                "the relation keeps its meaning in the reversed order"
            );
        });
        review_canvas.read_with(cx, |review_canvas, _| {
            let (frames, edges, _) = summary(review_canvas);
            assert_eq!(
                frames,
                [
                    ("internal/catalog".to_string(), 0),
                    ("internal/money".to_string(), 1)
                ]
            );
            assert_eq!(
                edges,
                [("internal/catalog".to_string(), "internal/money".to_string())]
            );
        });

        // Without a focused node, the frame that the user selected is active.
        review_canvas.update_in(cx, |review_canvas, window, cx| {
            review_canvas.focus_handle.focus(window, cx);
            review_canvas.selected_frame = Some("internal/money".into());
        });
        draw(cx);
        review_canvas.read_with(cx, |review_canvas, _| {
            assert_eq!(
                active_title(review_canvas).as_deref(),
                Some("internal/money")
            );
            assert_eq!(
                review_canvas
                    .model
                    .edge_relations(review_canvas.active_frame),
                [Relation::User]
            );
        });

        review_canvas.update(cx, |review_canvas, cx| {
            review_canvas.selected_frame = None;
            cx.notify();
        });
        draw(cx);
        review_canvas.read_with(cx, |review_canvas, _| {
            assert_eq!(review_canvas.active_frame, None);
        });
    }

    #[gpui::test]
    async fn test_failed_go_list_shows_a_warning_and_directory_frames(cx: &mut TestAppContext) {
        init_test(cx);
        let (fs, review_canvas, cx) = open_review_canvas(
            json!({
                ".git": {},
                "internal": { "money": { "money.go": "package money\n\n// new\n" } },
            }),
            cx,
        )
        .await;
        review_canvas.update(cx, |review_canvas, _| {
            review_canvas.go_list_output_for_tests = Some("not json".to_string());
        });
        let base = [("internal/money/money.go", "package money\n".to_string())];
        fs.set_head_for_repo(Path::new(path!("/project/.git")), &base, "sha");
        fs.set_merge_base_content_for_repo(Path::new(path!("/project/.git")), &base);
        draw(cx);

        review_canvas.read_with(cx, |review_canvas, _| {
            assert!(
                review_canvas
                    .graph_warning
                    .as_ref()
                    .is_some_and(|warning| warning.starts_with("No package graph")),
                "unexpected warning {:?}",
                review_canvas.graph_warning
            );
            let titles: Vec<&str> = review_canvas
                .model
                .frames
                .iter()
                .map(|frame| frame.title.as_ref())
                .collect();
            assert_eq!(titles, ["internal/money"]);
            assert!(review_canvas.model.edges.is_empty());
        });
    }

    #[gpui::test]
    async fn test_canvas_culls_and_zooms(cx: &mut TestAppContext) {
        const PACKAGE_COUNT: usize = 6;
        const FILES_PER_PACKAGE: usize = 10;
        const FILE_COUNT: usize = PACKAGE_COUNT * FILES_PER_PACKAGE;
        init_test(cx);

        let package_name = |package: usize| format!("pkg_{package}");
        let file_name = |file: usize| format!("file_{file:02}.go");
        let file_text = |package: usize, changed: bool| -> String {
            let mut text = format!("package {}\n", package_name(package));
            for line in 0..80 {
                if changed && line % 30 == 5 {
                    text.push_str(&format!("// changed line {line}\n"));
                } else {
                    text.push_str(&format!("// line {line}\n"));
                }
            }
            text
        };

        let mut tree: Map<String, Value> = Map::new();
        tree.insert(".git".into(), json!({}));
        let mut paths = Vec::new();
        let mut go_list_output = String::new();
        for package in 0..PACKAGE_COUNT {
            let files: Map<String, Value> = (0..FILES_PER_PACKAGE)
                .map(|file| (file_name(file), Value::String(file_text(package, true))))
                .collect();
            tree.insert(package_name(package), Value::Object(files));
            for file in 0..FILES_PER_PACKAGE {
                paths.push((
                    format!("{}/{}", package_name(package), file_name(file)),
                    package,
                ));
            }
            let imports: Vec<String> = package
                .checked_sub(1)
                .map(|previous| format!("example.com/w/{}", package_name(previous)))
                .into_iter()
                .collect();
            go_list_output.push_str(
                &json!({
                    "ImportPath": format!("example.com/w/{}", package_name(package)),
                    "Dir": format!("/project/{}", package_name(package)),
                    "Module": { "Path": "example.com/w" },
                    "GoFiles": (0..FILES_PER_PACKAGE).map(file_name).collect::<Vec<_>>(),
                    "Imports": imports,
                })
                .to_string(),
            );
        }
        let (fs, review_canvas, cx) = open_review_canvas(Value::Object(tree), cx).await;
        review_canvas.update(cx, |review_canvas, _| {
            review_canvas.go_list_output_for_tests = Some(go_list_output);
        });

        let head: Vec<(&str, String)> = paths
            .iter()
            .map(|(path, package)| (path.as_str(), file_text(*package, true)))
            .collect();
        let merge_base: Vec<(&str, String)> = paths
            .iter()
            .map(|(path, package)| (path.as_str(), file_text(*package, false)))
            .collect();
        fs.set_head_for_repo(Path::new(path!("/project/.git")), &head, "sha");
        fs.set_merge_base_content_for_repo(Path::new(path!("/project/.git")), &merge_base);
        draw(cx);

        let painted_at_full_zoom = review_canvas.read_with(cx, |review_canvas, _| {
            assert_eq!(review_canvas.nodes.len(), FILE_COUNT);
            assert_eq!(review_canvas.model.frames.len(), PACKAGE_COUNT);
            assert_eq!(review_canvas.model.edges.len(), PACKAGE_COUNT - 1);
            assert!(review_canvas.painted_node_count > 0);
            assert!(
                review_canvas.painted_node_count < FILE_COUNT,
                "nodes outside the view must be culled, but {} were painted",
                review_canvas.painted_node_count
            );
            review_canvas.painted_node_count
        });

        review_canvas.update(cx, |review_canvas, cx| {
            review_canvas.set_zoom_step(MIN_ZOOM_STEP, None, cx)
        });
        draw(cx);
        review_canvas.read_with(cx, |review_canvas, cx| {
            assert!(review_canvas.screen_font_size(cx) < LOW_DETAIL_FONT_SIZE);
            assert!(review_canvas.painted_node_count > painted_at_full_zoom);
        });

        review_canvas.update(cx, |review_canvas, cx| review_canvas.zoom_to_fit(cx));
        draw(cx);
        review_canvas.read_with(cx, |review_canvas, _| {
            assert_eq!(review_canvas.painted_node_count, FILE_COUNT);
        });
    }

    /// Opens the real `feature/discounts` branch of `test-repos/shop` with real git and the real
    /// `go list`. Make the repository with `test-repos/make-shop-repo.sh`, then run:
    /// `cargo test -p review_canvas -- --ignored`
    #[gpui::test]
    #[ignore = "needs test-repos/shop on the feature/discounts branch, and go on the PATH"]
    async fn test_real_shop_repository(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../test-repos/shop");
        let repository = repository
            .canonicalize()
            .expect("run test-repos/make-shop-repo.sh first");

        let project = Project::test(
            fs::RealFs::new(None, cx.executor()),
            [repository.as_path()],
            cx,
        )
        .await;
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            ReviewCanvas::open(workspace, window, cx)
        });
        let review_canvas = workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_item_as::<ReviewCanvas>(cx)
            })
            .expect("the review canvas is the active item");
        review_canvas.update(cx, |review_canvas, _| {
            review_canvas.go_list_output_for_tests = None;
        });

        let is_complete = |review_canvas: &ReviewCanvas| {
            review_canvas.load_state != LoadState::Loading
                && (review_canvas.go_packages.is_some() || review_canvas.graph_warning.is_some())
        };
        for _ in 0..400 {
            draw(cx);
            if review_canvas.read_with(cx, |review_canvas, _| is_complete(review_canvas)) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        draw(cx);

        review_canvas.read_with(cx, |review_canvas, cx| {
            assert_eq!(review_canvas.load_state, LoadState::Ready);
            assert_eq!(review_canvas.base_ref.as_deref(), Some("origin/main"));
            assert_eq!(review_canvas.graph_warning, None);
            assert_eq!(review_canvas.nodes.len(), 16);

            let frames: Vec<(String, usize)> = review_canvas
                .model
                .frames
                .iter()
                .map(|frame| (frame.title.to_string(), frame.column))
                .collect();
            assert_eq!(
                frames,
                [
                    ("internal/legacy", 0),
                    ("internal/money", 0),
                    ("internal/notify", 0),
                    ("internal/catalog", 1),
                    ("internal/payments", 1),
                    ("internal/orders", 2),
                    ("internal/orders_test", 3),
                    ("api", 3),
                ]
                .map(|(title, column)| (title.to_string(), column))
            );
            assert_eq!(review_canvas.model.edges.len(), 7);
            assert_eq!(review_canvas.model.other_files.len(), 3);

            for node in &review_canvas.nodes {
                let snapshot = node.editor.read(cx).buffer().read(cx).snapshot(cx);
                assert!(
                    snapshot.diff_hunks().next().is_some(),
                    "{} shows no diff",
                    node.info.path
                );
            }
        });
    }
}
