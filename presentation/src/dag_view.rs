use crate::keymap::{Command, Keymap, Scope};
use gpui::{prelude::*, *};
use infrastructure::{
    client::{Attempt, Coordinator, Run, Task, TaskState},
    theme::{Colors, blend},
};
use std::{
    cell::Cell,
    collections::{HashMap, HashSet, VecDeque},
    rc::Rc,
};

const NODE_WIDTH: f32 = 182.;
const NODE_HEIGHT: f32 = 84.;
const COLUMN_GAP: f32 = 66.;
const ROW_GAP: f32 = 22.;
const PADDING: f32 = 20.;
const ZOOM_STEP: f32 = 1.2;
const MIN_ZOOM: f32 = 0.25;
const MAX_ZOOM: f32 = 3.;
const DRAG_THRESHOLD: f64 = 4.;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Position {
    depth: usize,
    row: usize,
}

impl Position {
    fn origin(self) -> Point<Pixels> {
        point(
            px(PADDING + self.depth as f32 * (NODE_WIDTH + COLUMN_GAP)),
            px(PADDING + self.row as f32 * (NODE_HEIGHT + ROW_GAP)),
        )
    }
}

struct GraphLayout {
    positions: Vec<Position>,
    edges: Vec<(usize, usize)>,
    rows: usize,
}

struct GraphEdge {
    from: Point<Pixels>,
    to: Point<Pixels>,
    distance: usize,
    selected: bool,
}

/// Assign each task to its longest prerequisite depth and keep insertion order within a layer.
fn layout(tasks: &[Task]) -> GraphLayout {
    let ids: HashMap<_, _> = tasks
        .iter()
        .enumerate()
        .map(|(i, task)| (task.id.as_str(), i))
        .collect();
    let mut incoming = vec![0; tasks.len()];
    let mut outgoing = vec![Vec::new(); tasks.len()];
    let mut edges = Vec::new();
    for (target, task) in tasks.iter().enumerate() {
        for source in task
            .deps
            .iter()
            .filter_map(|id| ids.get(id.as_str()).copied())
        {
            incoming[target] += 1;
            outgoing[source].push(target);
            edges.push((source, target));
        }
    }

    let mut pending: VecDeque<_> = incoming
        .iter()
        .enumerate()
        .filter_map(|(index, &count)| (count == 0).then_some(index))
        .collect();
    let mut depths = vec![0; tasks.len()];
    while let Some(source) = pending.pop_front() {
        for &target in &outgoing[source] {
            depths[target] = depths[target].max(depths[source] + 1);
            incoming[target] -= 1;
            if incoming[target] == 0 {
                pending.push_back(target);
            }
        }
    }

    let columns = depths.iter().max().map_or(0, |depth| depth + 1);
    let mut counts = vec![0; columns];
    let positions = depths
        .into_iter()
        .map(|depth| {
            let row = counts[depth];
            counts[depth] += 1;
            Position { depth, row }
        })
        .collect();
    let rows = counts.into_iter().max().unwrap_or_default();
    GraphLayout {
        positions,
        edges,
        rows,
    }
}

impl GraphLayout {
    fn first_node(&self) -> usize {
        self.positions
            .iter()
            .position(|&position| position == Position { depth: 0, row: 0 })
            .unwrap_or_default()
    }

    fn neighbor(&self, selected: usize, command: Command) -> Option<usize> {
        let &Position { depth, row } = self.positions.get(selected)?;
        match command {
            Command::DagUp | Command::DagDown => {
                let row = if command == Command::DagUp {
                    row.checked_sub(1)?
                } else {
                    row + 1
                };
                self.positions
                    .iter()
                    .position(|&position| position == Position { depth, row })
            }
            Command::DagLeft | Command::DagRight => {
                let left = command == Command::DagLeft;
                let column = if left {
                    depth.checked_sub(1)?
                } else {
                    depth + 1
                };
                let connected: HashSet<_> = self
                    .edges
                    .iter()
                    .filter_map(|&(source, target)| {
                        if left {
                            (target == selected).then_some(source)
                        } else {
                            (source == selected).then_some(target)
                        }
                    })
                    .collect();
                self.positions
                    .iter()
                    .enumerate()
                    .filter(|(_, position)| position.depth == column)
                    .min_by_key(|&(candidate, position)| {
                        (!connected.contains(&candidate), row.abs_diff(position.row))
                    })
                    .map(|(index, _)| index)
            }
            _ => None,
        }
    }
}

pub(super) enum GraphAction {
    Select(usize),
    Zoom { steps: f32, position: Point<Pixels> },
    Press(Point<Pixels>),
    Move(Option<Point<Pixels>>),
    Release,
}

struct GraphDrag {
    position: Point<Pixels>,
    panning: bool,
}

struct GraphViewport {
    bounds: Rc<Cell<Bounds<Pixels>>>,
    offset: Point<Pixels>,
    scale: f32,
    drag: Option<GraphDrag>,
}

impl GraphViewport {
    fn new() -> Self {
        Self {
            bounds: Rc::default(),
            offset: Point::default(),
            scale: 1.,
            drag: None,
        }
    }

    fn zoom(&mut self, steps: f32, position: Point<Pixels>) -> bool {
        let scale = (self.scale * ZOOM_STEP.powf(steps)).clamp(MIN_ZOOM, MAX_ZOOM);
        if scale == self.scale {
            return false;
        }
        let anchor = position - self.bounds.get().origin;
        self.offset = anchor + (self.offset - anchor) * (scale / self.scale);
        self.scale = scale;
        true
    }

    fn panning(&self) -> bool {
        self.drag.as_ref().is_some_and(|drag| drag.panning)
    }

    fn pan(&mut self, position: Option<Point<Pixels>>) -> bool {
        let Some(position) = position else {
            return self.release();
        };
        let Some(drag) = &mut self.drag else {
            return false;
        };
        let delta = position - drag.position;
        if delta == Point::default() || (!drag.panning && delta.magnitude() < DRAG_THRESHOLD) {
            return false;
        }
        self.offset += delta;
        drag.position = position;
        drag.panning = true;
        true
    }

    fn release(&mut self) -> bool {
        self.drag.take().is_some_and(|drag| drag.panning)
    }

    fn reveal(&mut self, position: Position) {
        let viewport = self.bounds.get().size;
        if viewport.width <= px(0.) || viewport.height <= px(0.) {
            return;
        }
        let node = position.origin() * self.scale;
        let origin = node + self.offset;
        let max_x = (viewport.width - px(NODE_WIDTH * self.scale)).max(px(0.));
        let max_y = (viewport.height - px(NODE_HEIGHT * self.scale)).max(px(0.));
        self.offset = point(origin.x.clamp(px(0.), max_x), origin.y.clamp(px(0.), max_y)) - node;
    }
}

pub(super) struct DagView {
    pub id: u64,
    pub run: Run,
    pub focus: FocusHandle,
    pub opening: bool,
    pub message: Option<String>,
    pub refresh_error: Option<String>,
    graph: GraphLayout,
    selected: usize,
    viewport: GraphViewport,
}

impl DagView {
    pub fn new(id: u64, run: Run, focus: FocusHandle) -> Self {
        let graph = layout(&run.tasks);
        let selected = graph.first_node();
        Self {
            id,
            graph,
            run,
            focus,
            opening: false,
            message: None,
            refresh_error: None,
            selected,
            viewport: GraphViewport::new(),
        }
    }

    pub fn set_run(&mut self, run: Run) {
        let old_position = self.graph.positions.get(self.selected).copied();
        let graph = layout(&run.tasks);
        self.selected = self
            .task()
            .and_then(|task| run.tasks.iter().position(|new| new.id == task.id))
            .unwrap_or_else(|| graph.first_node());
        self.graph = graph;
        self.run = run;
        if let Some(&position) = self.graph.positions.get(self.selected)
            && Some(position) != old_position
        {
            self.viewport.reveal(position);
        }
    }

    pub fn step(&mut self, command: Command) {
        if let Some(index) = self.graph.neighbor(self.selected, command) {
            self.select(index);
        }
    }

    fn select(&mut self, index: usize) {
        self.selected = index;
        self.message = None;
        self.viewport.reveal(self.graph.positions[index]);
    }

    pub fn zoom(&mut self, steps: f32) -> bool {
        let Some(position) = self.graph.positions.get(self.selected) else {
            return false;
        };
        let bounds = self.viewport.bounds.get();
        let node_center = bounds.origin
            + self.viewport.offset
            + (position.origin() + point(px(NODE_WIDTH / 2.), px(NODE_HEIGHT / 2.)))
                * self.viewport.scale;
        let anchor = if bounds.contains(&node_center) {
            node_center
        } else {
            bounds.center()
        };
        self.viewport.zoom(steps, anchor)
    }

    pub fn graph_action(&mut self, action: GraphAction) -> bool {
        match action {
            GraphAction::Select(index) => {
                if self.opening || self.viewport.panning() || index >= self.graph.positions.len() {
                    return false;
                }
                self.select(index);
                true
            }
            GraphAction::Zoom { steps, position } => self.viewport.zoom(steps, position),
            GraphAction::Press(position) => {
                let panning = self.viewport.panning();
                self.viewport.drag = Some(GraphDrag {
                    position,
                    panning: false,
                });
                panning
            }
            GraphAction::Move(position) => self.viewport.pan(position),
            GraphAction::Release => self.viewport.release(),
        }
    }

    pub fn task(&self) -> Option<&Task> {
        self.run.tasks.get(self.selected)
    }

    pub fn attempt(&self) -> Option<&Attempt> {
        self.task().and_then(current_attempt)
    }

    pub fn render(
        &self,
        colors: &'static Colors,
        keymap: &Keymap,
        graph_action: impl Fn(GraphAction, &mut App) + Clone + 'static,
    ) -> impl IntoElement {
        let coordinator = match &self.run.coordinator {
            Coordinator::Agent { agent } => agent,
            Coordinator::External { label } => label,
        };
        let open_key = keymap.keys(Scope::Dag, Command::DagOpen);
        let close_key = keymap.first(Scope::Dag, Command::DagClose);
        let mut actions = Vec::new();
        if !open_key.is_empty()
            && self
                .attempt()
                .is_some_and(|attempt| attempt.session.is_some())
        {
            actions.push(format!("{open_key} open worker"));
        }
        if self.task().is_some() {
            let zoom_keys = [
                keymap.keys(Scope::Dag, Command::DagZoomIn),
                keymap.keys(Scope::Dag, Command::DagZoomOut),
            ];
            let zoom_controls = std::iter::once("Wheel")
                .chain(
                    zoom_keys
                        .iter()
                        .map(String::as_str)
                        .filter(|key| !key.is_empty()),
                )
                .collect::<Vec<_>>()
                .join(" / ");
            actions.push(format!("{zoom_controls} zoom"));
            actions.push("Drag to pan".into());
        }
        actions.push(format!("{close_key} close"));
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba((colors.background << 8) | 0x88))
            .flex()
            .justify_center()
            .px_4()
            .py_6()
            .child(
                div()
                    .id("dag-modal")
                    .track_focus(&self.focus)
                    .w_full()
                    .h_full()
                    .max_w(px(1120.))
                    .flex()
                    .flex_col()
                    .bg(rgb(colors.surface))
                    .border_1()
                    .border_color(rgb(colors.border))
                    .shadow_lg()
                    .child(
                        div()
                            .flex_none()
                            .px_3()
                            .py_2()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(self.run.title.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(colors.muted))
                                    .child(format!("Coordinator · {coordinator}")),
                            ),
                    )
                    .child(if let Some(task) = self.task() {
                        div()
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .border_y_1()
                            .border_color(rgb(colors.border))
                            .child(self.graph_elements(colors, graph_action))
                            .child(details(colors, task))
                            .into_any_element()
                    } else {
                        div()
                            .flex()
                            .flex_1()
                            .items_center()
                            .justify_center()
                            .text_color(rgb(colors.muted))
                            .child("This run has no tasks.")
                            .into_any_element()
                    })
                    .child(
                        div()
                            .flex_none()
                            .px_3()
                            .py_2()
                            .text_xs()
                            .text_color(rgb(colors.muted))
                            .children(self.refresh_error.iter().chain(self.message.iter()).map(
                                |message| {
                                    div()
                                        .text_color(rgb(
                                            colors.readable(colors.indexed[3], colors.surface)
                                        ))
                                        .child(message.clone())
                                },
                            ))
                            .when(self.opening, |footer| {
                                footer.child(div().child("Opening worker terminal…"))
                            })
                            .child(actions.join(" · ")),
                    ),
            )
    }

    fn graph_elements(
        &self,
        colors: &'static Colors,
        action: impl Fn(GraphAction, &mut App) + Clone + 'static,
    ) -> impl IntoElement {
        let lane = px(PADDING + self.graph.rows as f32 * (NODE_HEIGHT + ROW_GAP));
        let scale = self.viewport.scale;
        let offset = self.viewport.offset;
        let viewport_bounds = self.viewport.bounds.clone();
        let panning = self.viewport.panning();
        let mut edges: Vec<_> = self
            .graph
            .edges
            .iter()
            .filter_map(|&(source, target)| {
                let from = self.graph.positions[source];
                let to = self.graph.positions[target];
                let distance = to.depth.checked_sub(from.depth)?;
                (distance > 0).then(|| GraphEdge {
                    from: from.origin() + point(px(NODE_WIDTH), px(NODE_HEIGHT / 2.)),
                    to: to.origin() + point(px(0.), px(NODE_HEIGHT / 2.)),
                    distance,
                    selected: source == self.selected || target == self.selected,
                })
            })
            .collect();
        edges.sort_by_key(|edge| edge.selected);

        div()
            .id("dag-graph")
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(colors.background))
            .cursor(if panning {
                CursorStyle::ClosedHand
            } else {
                CursorStyle::OpenHand
            })
            .on_scroll_wheel({
                let action = action.clone();
                move |event, _, cx| {
                    let steps = f32::from(event.delta.pixel_delta(px(16.)).y) / 48.;
                    if steps != 0. {
                        action(
                            GraphAction::Zoom {
                                steps,
                                position: event.position,
                            },
                            cx,
                        );
                    }
                    cx.stop_propagation();
                }
            })
            .on_mouse_down(MouseButton::Left, {
                let action = action.clone();
                move |event, _, cx| action(GraphAction::Press(event.position), cx)
            })
            .on_mouse_up(MouseButton::Left, {
                let action = action.clone();
                // Node clicks bubble first and can tell whether this press became a drag.
                move |_, _, cx| action(GraphAction::Release, cx)
            })
            .on_mouse_up_out(MouseButton::Left, {
                let action = action.clone();
                move |_, _, cx| action(GraphAction::Release, cx)
            })
            .child(
                canvas(move |bounds, _, _| viewport_bounds.set(bounds), {
                    let action = action.clone();
                    move |bounds, (), window, _| {
                        // Keep panning while the pointer crosses the edge of the graph.
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                            if phase.capture() {
                                let position = event.dragging().then_some(event.position);
                                action(GraphAction::Move(position), cx);
                            }
                        });
                        for edge in edges {
                            edge.paint(window, bounds.origin + offset, scale, lane, colors);
                        }
                    }
                })
                .absolute()
                .inset_0(),
            )
            .children(self.run.tasks.iter().enumerate().map(|(index, task)| {
                let action = action.clone();
                let origin = self.graph.positions[index].origin() * scale + offset;
                let (label, tint) = task_style(colors, task.state);
                let fill = blend(colors.surface, tint, 18);
                let color = colors.readable(tint, fill);
                let model = current_attempt(task).and_then(|attempt| attempt.model.clone());
                div()
                    .id(("dag-node", index))
                    .absolute()
                    .left(origin.x)
                    .top(origin.y)
                    .w(px(NODE_WIDTH * scale))
                    .h(px(NODE_HEIGHT * scale))
                    .px(rems(0.5 * scale))
                    .py(rems(0.25 * scale))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .gap(rems(0.25 * scale))
                    .border(px(2. * scale))
                    .border_color(if index == self.selected {
                        rgb(colors.foreground)
                    } else {
                        rgb(tint)
                    })
                    .bg(rgb(fill))
                    .when(!panning, |node| node.cursor_pointer())
                    .child(
                        div()
                            .text_size(rems(0.875 * scale))
                            .truncate()
                            .child(task.title.clone()),
                    )
                    .child(
                        div()
                            .text_size(rems(0.75 * scale))
                            .text_color(rgb(color))
                            .child(label),
                    )
                    .when_some(model, |node, model| {
                        node.child(
                            div()
                                .text_size(rems(0.75 * scale))
                                .text_color(rgb(colors.readable(colors.muted, fill)))
                                .truncate()
                                .child(model),
                        )
                    })
                    .on_click(move |_, _, cx| action(GraphAction::Select(index), cx))
            }))
    }
}

impl GraphEdge {
    fn paint(
        self,
        window: &mut Window,
        origin: Point<Pixels>,
        scale: f32,
        lane: Pixels,
        colors: &Colors,
    ) {
        let Self {
            from,
            to,
            distance,
            selected,
        } = self;
        let tip = to - point(px(1.), px(0.));
        let mut path = PathBuilder::stroke(px(scale * if selected { 2. } else { 1. }));
        path.move_to(from);
        if distance == 1 {
            let bend = point(px(COLUMN_GAP / 2.), px(0.));
            path.cubic_bezier_to(tip, from + bend, tip - bend);
        } else {
            // Bypass intervening cards through the space below the layers.
            let start = from.x + px(10.);
            let end = to.x - px(10.);
            for point in [
                point(start, from.y),
                point(start, lane),
                point(end, lane),
                point(end, to.y),
                tip,
            ] {
                path.line_to(point);
            }
        }
        path.move_to(tip - point(px(6.), px(4.)));
        path.line_to(tip);
        path.line_to(tip - point(px(6.), px(-4.)));
        path.scale(scale);
        path.translate(origin);
        if let Ok(path) = path.build() {
            window.paint_path(
                path,
                rgb(if selected {
                    colors.accent
                } else {
                    colors.muted
                }),
            );
        }
    }
}

fn current_attempt(task: &Task) -> Option<&Attempt> {
    let id = task.current_attempt.as_deref()?;
    task.attempts.iter().find(|attempt| attempt.id == id)
}

fn details(colors: &Colors, task: &Task) -> impl IntoElement {
    let mut fields = Vec::new();
    if let Some(attempt) = current_attempt(task) {
        fields.push((
            "Assigned agent",
            format!("{} · {}", attempt.provider.name(), attempt.agent),
        ));
        if let Some(model) = &attempt.model {
            fields.push(("Model", model.clone()));
        }
        fields.extend([
            ("Worktree", attempt.worktree.display().to_string()),
            (
                "Session",
                attempt
                    .session
                    .clone()
                    .unwrap_or_else(|| "Unavailable".into()),
            ),
            ("Attempt", attempt.number.to_string()),
        ]);
        fields.extend(
            [
                ("Result", attempt.summary.clone()),
                ("Blocker", attempt.blocker.clone()),
                ("Interruption", attempt.detail.clone()),
            ]
            .into_iter()
            .filter_map(|(label, value)| Some((label, value?))),
        );
    }
    let (state, tint) = task_style(colors, task.state);
    let color = colors.readable(tint, colors.surface);
    div()
        .id("dag-details")
        .w(relative(0.34))
        .max_w(px(310.))
        .min_h_0()
        .flex_none()
        .overflow_y_scroll()
        .border_l_1()
        .border_color(rgb(colors.border))
        .p_3()
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(task.title.clone()),
        )
        .child(div().text_xs().text_color(rgb(color)).child(state))
        .when(fields.is_empty(), |panel| {
            panel.child(
                div()
                    .mt_3()
                    .text_sm()
                    .text_color(rgb(colors.muted))
                    .child("No attempt yet."),
            )
        })
        .children(fields.into_iter().map(|(label, text)| {
            div()
                .mt_3()
                .child(div().text_xs().text_color(rgb(colors.muted)).child(label))
                .child(div().text_sm().child(text))
        }))
}

fn task_style(colors: &Colors, state: TaskState) -> (&'static str, u32) {
    match state {
        TaskState::Waiting => ("Waiting", colors.muted),
        TaskState::Ready => ("Ready", colors.indexed[6]),
        TaskState::Working => ("Working", colors.accent),
        TaskState::NeedsInput => ("Needs input", colors.indexed[3]),
        TaskState::Completed => ("Completed", colors.indexed[2]),
        TaskState::Failed => ("Failed", colors.error),
        TaskState::Interrupted => ("Interrupted", colors.indexed[5]),
    }
}

#[cfg(test)]
mod tests {
    use super::{GraphDrag, GraphViewport, MAX_ZOOM, MIN_ZOOM, Position, layout};
    use crate::keymap::Command;
    use gpui::{Bounds, point, px, size};
    use infrastructure::client::{Task, TaskState};

    fn task(id: &str, deps: &[&str]) -> Task {
        Task {
            id: id.into(),
            title: id.into(),
            instructions: String::new(),
            deps: deps.iter().copied().map(str::to_owned).collect(),
            state: TaskState::Waiting,
            current_attempt: None,
            attempts: Vec::new(),
        }
    }

    #[test]
    fn longest_path_sets_depth_and_keeps_insertion_order_within_layers() {
        let graph = layout(&[
            task("deliver", &["prepare", "review"]),
            task("review", &["prepare", "build"]),
            task("prepare", &[]),
            task("build", &[]),
            task("standalone", &["missing"]),
        ]);
        assert_eq!(
            graph.positions,
            [
                Position { depth: 2, row: 0 },
                Position { depth: 1, row: 0 },
                Position { depth: 0, row: 0 },
                Position { depth: 0, row: 1 },
                Position { depth: 0, row: 2 },
            ]
        );
        assert_eq!(graph.edges, [(2, 0), (1, 0), (2, 1), (3, 1)]);
        assert_eq!(graph.rows, 3);
        assert_eq!(graph.first_node(), 2);
        assert!(layout(&[]).positions.is_empty());
    }

    #[test]
    fn navigation_prefers_a_connected_node_over_a_closer_unrelated_node() {
        let graph = layout(&[
            task("other", &[]),
            task("parent", &[]),
            task("child", &["parent"]),
        ]);
        assert_eq!(graph.neighbor(0, Command::DagDown), Some(1));
        assert_eq!(graph.neighbor(1, Command::DagUp), Some(0));
        assert_eq!(graph.neighbor(1, Command::DagDown), None);
        assert_eq!(graph.neighbor(2, Command::DagLeft), Some(1));
        assert_eq!(graph.neighbor(1, Command::DagRight), Some(2));
        assert_eq!(graph.neighbor(2, Command::DagRight), None);
    }

    #[test]
    fn zoom_keeps_the_graph_point_under_the_pointer_even_at_the_limits() {
        let mut viewport = GraphViewport::new();
        viewport.bounds.set(Bounds {
            origin: point(px(30.), px(60.)),
            size: size(px(600.), px(400.)),
        });
        viewport.offset = point(px(-25.), px(50.));
        let anchor = point(px(150.), px(90.));
        let graph_point = anchor - viewport.offset;
        let pointer = viewport.bounds.get().origin + anchor;
        for (steps, expected) in [(100., MAX_ZOOM), (-100., MIN_ZOOM)] {
            assert!(viewport.zoom(steps, pointer));
            assert_eq!(viewport.scale, expected);
            assert_eq!(viewport.offset + graph_point * viewport.scale, anchor);
            assert!(!viewport.zoom(steps, pointer));
        }
    }

    #[test]
    fn dragging_ignores_click_jitter_and_moves_in_both_directions() {
        let mut viewport = GraphViewport::new();
        let start = point(px(50.), px(30.));
        viewport.drag = Some(GraphDrag {
            position: start,
            panning: false,
        });
        assert!(!viewport.pan(Some(start + point(px(2.), px(1.)))));
        assert!(!viewport.panning());
        let end = start + point(px(-40.), px(20.));
        assert!(viewport.pan(Some(end)));
        assert!(viewport.panning());
        assert_eq!(viewport.offset, end - start);
        assert!(viewport.pan(Some(start)));
        assert!(viewport.panning());
        assert!(viewport.release());
        assert!(!viewport.panning());
        assert!(!viewport.pan(Some(end)));
        assert_eq!(viewport.offset, Default::default());
    }
}
