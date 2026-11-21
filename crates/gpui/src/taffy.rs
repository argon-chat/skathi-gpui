mod layout_bounds;
mod retained;
pub use retained::VnRetainedLayoutStats;

use layout_bounds::LayoutIdMap;

use crate::{
    AbsoluteLength, App, Bounds, DefiniteLength, Display, Edges, GridTemplate, Length, Pixels,
    Point, Size, Style, Window, size,
    util::{
        ceil_to_device_pixel, round_half_toward_zero, round_stroke_to_device_pixel,
        round_to_device_pixel,
    },
};
use collections::{FxHashMap, FxHashSet};
use std::{fmt::Debug, ops::Range};
use taffy::{
    TaffyTree, TraversePartialTree as _,
    geometry::{Point as TaffyPoint, Rect as TaffyRect, Size as TaffySize},
    prelude::{max_content, min_content},
    style::AvailableSpace as TaffyAvailableSpace,
    tree::NodeId,
};

#[cfg(feature = "stacker")]
type StackSafe<T> = stacksafe::StackSafe<T>;
#[cfg(not(feature = "stacker"))]
type StackSafe<T> = T;

type MeasureFn =
    dyn FnMut(Size<Option<Pixels>>, Size<AvailableSpace>, &mut Window, &mut App) -> Size<Pixels>;
type NodeMeasureFn = StackSafe<Box<MeasureFn>>;

struct NodeContext {
    measure: NodeMeasureFn,
}
pub struct TaffyLayoutEngine {
    taffy: TaffyTree<NodeContext>,
    absolute_layout_bounds: LayoutIdMap<Bounds<Pixels>>,
    /// Unrounded absolute border-box top-left per-node coordinate in device pixels.
    absolute_outer_origins: LayoutIdMap<Point<f32>>,
    retention: retained::Retention,
    computed_layouts: FxHashSet<LayoutId>,
    layout_bounds_scratch_space: Vec<LayoutId>,
    /// This frame's nonzero `vn_order`s, by node.
    orders: FxHashMap<LayoutId, i32>,
    calc_sizes: Vec<CalcSizeNode>,
}

const EXPECT_MESSAGE: &str = "we should avoid taffy layout errors by construction if possible";

impl TaffyLayoutEngine {
    pub fn new() -> Self {
        let mut taffy = TaffyTree::new();
        taffy.disable_rounding();
        TaffyLayoutEngine {
            taffy,
            absolute_layout_bounds: LayoutIdMap::default(),
            absolute_outer_origins: LayoutIdMap::default(),
            retention: retained::Retention::default(),
            computed_layouts: FxHashSet::default(),
            layout_bounds_scratch_space: Vec::new(),
            orders: FxHashMap::default(),
            calc_sizes: Vec::new(),
        }
    }

    pub fn clear(&mut self) {
        if self.retention.enabled {
            retained::finish_frame(self);
        } else {
            self.taffy.clear();
        }
        self.absolute_layout_bounds.clear();
        self.absolute_outer_origins.clear();
        self.computed_layouts.clear();
        self.orders.clear();
        self.calc_sizes.clear();
    }

    pub(crate) fn retention_enabled(&self) -> bool {
        self.retention.enabled
    }

    pub(crate) fn enable_retention(&mut self) {
        if !self.retention.enabled {
            assert_eq!(
                self.taffy.total_node_count(),
                0,
                "enable retained layout before the first layout request"
            );
            self.retention.enabled = true;
        }
    }

    pub(crate) fn profile_retention(&mut self, enabled: bool) {
        self.retention.stats = enabled.then(Default::default);
    }

    pub(crate) fn take_retention_stats(&mut self) -> Option<VnRetainedLayoutStats> {
        self.retention.stats.as_mut().map(std::mem::take)
    }

    pub fn request_layout(
        &mut self,
        style: Style,
        rem_size: Pixels,
        scale_factor: f32,
        children: &[LayoutId],
    ) -> LayoutId {
        self.request_layout_in_scope(None, style, rem_size, scale_factor, children)
    }

    pub(crate) fn request_layout_in_scope(
        &mut self,
        scope: Option<crate::GlobalElementId>,
        style: Style,
        rem_size: Pixels,
        scale_factor: f32,
        children: &[LayoutId],
    ) -> LayoutId {
        let taffy_style = style.to_taffy(rem_size, scale_factor);
        let reordered = self.order_modified(&style, children);
        let children = reordered.as_deref().unwrap_or(children);
        let id = if self.retention.enabled {
            retained::request_layout(self, scope, taffy_style, children)
        } else if children.is_empty() {
            self.taffy
                .new_leaf(taffy_style)
                .expect(EXPECT_MESSAGE)
                .into()
        } else {
            self.taffy
                // This is safe because LayoutId is repr(transparent) to taffy::tree::NodeId.
                .new_with_children(taffy_style, LayoutId::to_taffy_slice(children))
                .expect(EXPECT_MESSAGE)
                .into()
        };
        self.note_order(id, &style);
        id
    }

    fn note_order(&mut self, id: LayoutId, style: &Style) {
        if style.vn_order != 0 {
            self.orders.insert(id, style.vn_order);
        }
        if let Some(node) = CalcSizeNode::of(id, style) {
            self.calc_sizes.push(node);
        }
    }

    /// CSS's order-modified document order for a flex or grid container's children; `None` when
    /// it is document order.
    fn order_modified(&self, style: &Style, children: &[LayoutId]) -> Option<Vec<LayoutId>> {
        if self.orders.is_empty() || !matches!(style.display, Display::Flex | Display::Grid) {
            return None;
        }
        let order = |id: &LayoutId| self.orders.get(id).copied().unwrap_or(0);
        if children.windows(2).all(|w| order(&w[0]) <= order(&w[1])) {
            return None;
        }
        let mut sorted = children.to_vec();
        sorted.sort_by_key(order);
        Some(sorted)
    }

    pub fn request_measured_layout(
        &mut self,
        style: Style,
        rem_size: Pixels,
        scale_factor: f32,
        measure: impl FnMut(
            Size<Option<Pixels>>,
            Size<AvailableSpace>,
            &mut Window,
            &mut App,
        ) -> Size<Pixels>
        + 'static,
    ) -> LayoutId {
        self.request_measured_layout_in_scope(None, style, rem_size, scale_factor, measure)
    }

    pub(crate) fn request_measured_layout_in_scope(
        &mut self,
        scope: Option<crate::GlobalElementId>,
        style: Style,
        rem_size: Pixels,
        scale_factor: f32,
        measure: impl FnMut(
            Size<Option<Pixels>>,
            Size<AvailableSpace>,
            &mut Window,
            &mut App,
        ) -> Size<Pixels>
        + 'static,
    ) -> LayoutId {
        let taffy_style = style.to_taffy(rem_size, scale_factor);
        let id = if self.retention.enabled {
            retained::request_measured_layout(self, scope, taffy_style, measure)
        } else {
            let measure = Box::new(measure) as Box<MeasureFn>;
            #[cfg(feature = "stacker")]
            let measure = StackSafe::new(measure);

            self.taffy
                .new_leaf_with_context(taffy_style, NodeContext { measure })
                .expect(EXPECT_MESSAGE)
                .into()
        };
        self.note_order(id, &style);
        id
    }

    /// Treats any `auto` dimension of the given node's style as filling `size`.
    ///
    /// This is applied to window roots before layout so they behave like the
    /// root element on the web, which stretches to fill the initial containing
    /// block (the viewport) unless given an explicit size. Explicitly styled
    /// dimensions are preserved.
    pub fn stretch_auto_size_to_fill(
        &mut self,
        id: LayoutId,
        size: Size<Pixels>,
        scale_factor: f32,
    ) {
        if self.retention.enabled {
            retained::stretch(self, id, size, scale_factor);
            return;
        }
        let style = self.taffy.style(id.0).expect(EXPECT_MESSAGE);
        let stretch_width = style.size.width.is_auto();
        let stretch_height = style.size.height.is_auto();
        if !stretch_width && !stretch_height {
            return;
        }
        let mut style = style.clone();
        if stretch_width {
            style.size.width =
                taffy::style::Dimension::length(round_to_device_pixel(size.width.0, scale_factor));
        }
        if stretch_height {
            style.size.height =
                taffy::style::Dimension::length(round_to_device_pixel(size.height.0, scale_factor));
        }
        self.taffy.set_style(id.0, style).expect(EXPECT_MESSAGE);
    }

    // Used to understand performance
    #[allow(dead_code)]
    fn count_all_children(&self, parent: LayoutId) -> anyhow::Result<u32> {
        let mut count = 0;

        for child in self.taffy.children(parent.0)? {
            // Count this child.
            count += 1;

            // Count all of this child's children.
            count += self.count_all_children(LayoutId(child))?
        }

        Ok(count)
    }

    // Used to understand performance
    #[allow(dead_code)]
    fn max_depth(&self, depth: u32, parent: LayoutId) -> anyhow::Result<u32> {
        println!(
            "{parent:?} at depth {depth} has {} children",
            self.taffy.child_count(parent.0)
        );

        let mut max_child_depth = 0;

        for child in self.taffy.children(parent.0)? {
            max_child_depth = std::cmp::max(max_child_depth, self.max_depth(0, LayoutId(child))?);
        }

        Ok(depth + 1 + max_child_depth)
    }

    // Used to understand performance
    #[allow(dead_code)]
    fn get_edges(&self, parent: LayoutId) -> anyhow::Result<Vec<(LayoutId, LayoutId)>> {
        let mut edges = Vec::new();

        for child in self.taffy.children(parent.0)? {
            edges.push((parent, LayoutId(child)));

            edges.extend(self.get_edges(LayoutId(child))?);
        }

        Ok(edges)
    }

    /// vue-native fork: `id` is laid out as a root, so it has no parent.
    ///
    /// A retained node keeps the Taffy parent it had last frame until that
    /// parent is requested again with other children, or freed, and an
    /// unrequested one is only freed when the frame ends. A node that was a
    /// child and is a root this frame would be placed at the stale parent's
    /// origin by [`Self::layout_bounds`].
    pub(crate) fn detach_root(&mut self, id: LayoutId) {
        if let Some(parent) = self.taffy.parent(id.0) {
            self.taffy.remove_child(parent, id.0).expect(EXPECT_MESSAGE);
        }
    }

    #[cfg_attr(feature = "stacker", stacksafe::stacksafe)]
    pub fn compute_layout(
        &mut self,
        id: LayoutId,
        available_space: Size<AvailableSpace>,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Leaving this here until we have a better instrumentation approach.
        // println!("Laying out {} children", self.count_all_children(id)?);
        // println!("Max layout depth: {}", self.max_depth(0, id)?);

        // Output the edges (branches) of the tree in Mermaid format for visualization.
        // println!("Edges:");
        // for (a, b) in self.get_edges(id)? {
        //     println!("N{} --> N{}", u64::from(a), u64::from(b));
        // }
        //

        if self.retention.enabled {
            self.detach_root(id);
        }

        if !self.computed_layouts.insert(id) {
            let stack = &mut self.layout_bounds_scratch_space;
            stack.push(id);
            while let Some(id) = stack.pop() {
                self.absolute_layout_bounds.remove(&id);
                self.absolute_outer_origins.remove(&id);
                stack.extend(
                    self.taffy
                        .children(id.into())
                        .expect(EXPECT_MESSAGE)
                        .into_iter()
                        .map(LayoutId::from),
                );
            }
        }

        let scale_factor = window.scale_factor();

        let transform = |v: AvailableSpace| match v {
            AvailableSpace::Definite(pixels) => {
                AvailableSpace::Definite(Pixels(pixels.0 * scale_factor))
            }
            AvailableSpace::MinContent => AvailableSpace::MinContent,
            AvailableSpace::MaxContent => AvailableSpace::MaxContent,
        };
        let available_space = size(
            transform(available_space.width),
            transform(available_space.height),
        );

        if self.retention.enabled {
            retained::prepare_compute(self, scale_factor);
            retained::replay(self, window, cx);
        }
        let calc_sizes = self.take_calc_sizes(id);
        if !calc_sizes.is_empty() {
            let requested = self.probe_calc_sizes(&calc_sizes);
            self.run_taffy(id, available_space, scale_factor, window, cx);
            let rem_size = window.rem_size();
            self.resolve_calc_sizes(&calc_sizes, requested, rem_size, scale_factor);
        }
        self.run_taffy(id, available_space, scale_factor, window, cx);
        if self.retention.enabled {
            retained::settle(self, window, cx);
        }
    }

    fn run_taffy(
        &mut self,
        id: LayoutId,
        available_space: Size<AvailableSpace>,
        scale_factor: f32,
        window: &mut Window,
        cx: &mut App,
    ) {
        let retention = &mut self.retention;
        self.taffy
            .compute_layout_with_measure(
                id.into(),
                available_space.into(),
                |input, id, node_context, style| {
                    taffy::compute_leaf_layout(
                        input,
                        style,
                        // vue-native fork: a leaf's padding, border and size
                        // can be a `DefiniteLength::Calc` too (taffy 0.15 asks
                        // the caller to resolve them here, not the tree).
                        resolve_calc_handle,
                        |known_dimensions, available_space| {
                            let Some(node_context) = node_context else {
                                return taffy::geometry::Size::default();
                            };

                            let known_dimensions = Size {
                                width: known_dimensions.width.map(|e| Pixels(e / scale_factor)),
                                height: known_dimensions.height.map(|e| Pixels(e / scale_factor)),
                            };

                            let available_space: Size<AvailableSpace> = available_space.into();
                            let untransform = |ev: AvailableSpace| match ev {
                                AvailableSpace::Definite(pixels) => {
                                    AvailableSpace::Definite(Pixels(pixels.0 / scale_factor))
                                }
                                AvailableSpace::MinContent => AvailableSpace::MinContent,
                                AvailableSpace::MaxContent => AvailableSpace::MaxContent,
                            };
                            let available_space = size(
                                untransform(available_space.width),
                                untransform(available_space.height),
                            );

                            if retention.enabled {
                                retained::note_measure(
                                    retention,
                                    id,
                                    known_dimensions,
                                    available_space,
                                );
                            }
                            let measured_size: Size<Pixels> = (node_context.measure)(
                                known_dimensions,
                                available_space,
                                window,
                                cx,
                            );
                            snap_measured_size_to_device_pixels(measured_size, scale_factor).into()
                        },
                    )
                },
            )
            .expect(EXPECT_MESSAGE);
    }

    /// vue-native fork: the `calc-size()` nodes under `root`; the others wait for their own root.
    fn take_calc_sizes(&mut self, root: LayoutId) -> Vec<CalcSizeNode> {
        if self.calc_sizes.is_empty() {
            return Vec::new();
        }
        let taffy = &self.taffy;
        let under = |mut node: NodeId| loop {
            if node == root.0 {
                return true;
            }
            match taffy.parent(node) {
                Some(parent) => node = parent,
                None => return false,
            }
        };
        let (mine, rest) = std::mem::take(&mut self.calc_sizes)
            .into_iter()
            .partition(|n| under(n.id.0));
        self.calc_sizes = rest;
        mine
    }

    /// The first pass lays each `calc-size()` axis out at its basis keyword, unclamped and (for a
    /// flex basis) unflexed, so the box's size there is the basis's `size`. Returns the styles
    /// as requested.
    fn probe_calc_sizes(&mut self, nodes: &[CalcSizeNode]) -> Vec<taffy::style::Style> {
        nodes
            .iter()
            .map(|node| {
                let requested = self.taffy.style(node.id.0).expect(EXPECT_MESSAGE).clone();
                let mut probe = requested.clone();
                for (axis, props) in [(0, &node.width), (1, &node.height)] {
                    let Some(first) = props.iter().flatten().next() else {
                        continue;
                    };
                    let size = basis_dimension(first.basis);
                    let none = taffy::style::LengthPercentageAuto::auto();
                    if axis == 0 {
                        probe.size.width = size;
                        probe.min_size.width = none;
                        probe.max_size.width = none;
                    } else {
                        probe.size.height = size;
                        probe.min_size.height = none;
                        probe.max_size.height = none;
                    }
                }
                if let Some(main) = self.main_axis(node.id) {
                    // `size` is the box's own size at the basis, not what the flex line makes of
                    // it: a container laid out again at last frame's size would shrink it.
                    let sized = [&node.width, &node.height][main]
                        .iter()
                        .any(Option::is_some);
                    if let Some(basis) = node.flex_basis {
                        probe.flex_basis = basis_dimension(basis.basis);
                    } else if sized {
                        probe.flex_basis = taffy::style::Dimension::auto();
                    }
                    if node.flex_basis.is_some() || sized {
                        probe.flex_grow = 0.;
                        probe.flex_shrink = 0.;
                    }
                }
                if probe != requested {
                    self.taffy
                        .set_style(node.id.0, probe)
                        .expect(EXPECT_MESSAGE);
                }
                requested
            })
            .collect()
    }

    /// The second pass's styles: each `calc-size()` as the length its function gives for the
    /// measured basis, in the box-sizing box.
    fn resolve_calc_sizes(
        &mut self,
        nodes: &[CalcSizeNode],
        requested: Vec<taffy::style::Style>,
        rem_size: Pixels,
        scale_factor: f32,
    ) {
        for (node, mut style) in nodes.iter().zip(requested) {
            let layout = *self.taffy.layout(node.id.0).expect(EXPECT_MESSAGE);
            let content_box = style.box_sizing == taffy::style::BoxSizing::ContentBox;
            let inner = |outer: f32, a: f32, b: f32, c: f32, d: f32| {
                if content_box {
                    (outer - a - b - c - d).max(0.)
                } else {
                    outer
                }
            };
            let measured = [
                inner(
                    layout.size.width,
                    layout.padding.left,
                    layout.padding.right,
                    layout.border.left,
                    layout.border.right,
                ),
                inner(
                    layout.size.height,
                    layout.padding.top,
                    layout.padding.bottom,
                    layout.border.top,
                    layout.border.bottom,
                ),
            ];
            let resolve = |c: &crate::VnCalcSize, axis: usize| {
                calc_size_value(c, measured[axis] / scale_factor, rem_size, scale_factor)
            };
            let dimension = |v: Result<*const (), f32>| match v {
                Ok(handle) => taffy::style::Dimension::calc(handle),
                Err(length) => taffy::style::Dimension::length(length),
            };
            let bound = |v: Result<*const (), f32>| match v {
                Ok(handle) => taffy::style::LengthPercentageAuto::calc(handle),
                Err(length) => taffy::style::LengthPercentageAuto::length(length),
            };
            for axis in 0..2 {
                let [size, min, max] = if axis == 0 { node.width } else { node.height };
                let (s, lo, hi) = if axis == 0 {
                    (
                        &mut style.size.width,
                        &mut style.min_size.width,
                        &mut style.max_size.width,
                    )
                } else {
                    (
                        &mut style.size.height,
                        &mut style.min_size.height,
                        &mut style.max_size.height,
                    )
                };
                if let Some(c) = size {
                    *s = dimension(resolve(&c, axis));
                }
                if let Some(c) = min {
                    *lo = bound(resolve(&c, axis));
                }
                if let Some(c) = max {
                    *hi = bound(resolve(&c, axis));
                }
            }
            if let (Some(c), Some(axis)) = (node.flex_basis, self.main_axis(node.id)) {
                style.flex_basis = dimension(resolve(&c, axis));
            }
            if self.taffy.style(node.id.0).expect(EXPECT_MESSAGE) != &style {
                self.taffy
                    .set_style(node.id.0, style)
                    .expect(EXPECT_MESSAGE);
            }
        }
    }

    /// The flex container's main axis for `id`, 0 for rows and 1 for columns.
    fn main_axis(&self, id: LayoutId) -> Option<usize> {
        let parent = self.taffy.parent(id.0)?;
        let style = self.taffy.style(parent).ok()?;
        use taffy::style::FlexDirection as F;
        (style.display == taffy::style::Display::Flex).then(|| match style.flex_direction {
            F::Row | F::RowReverse => 0,
            F::Column | F::ColumnReverse => 1,
        })
    }

    // Pixel snapping
    //
    // Painting primitives at non-integer pixel coordinates produces blurry
    // output. Pixel snapping converts layout coordinates into integer
    // device-pixel coordinates so painted edges land exactly on physical
    // pixel boundaries.
    //
    // Non-integer coordinates can arise for several reasons, including:
    //   - flex distribution, percentages, centering, and text measurement
    //     can produce fractional element sizes and positions;
    //   - at fractional scale factors (for example 125% or 150%), integer
    //     logical-pixel values can map to non-integer device-pixel values.
    //
    // We pixel-snap by rounding in device-pixel space, after multiplying
    // by `scale_factor`, so that snapping targets physical pixels. Bounds
    // are divided by `scale_factor` before being returned to GPUI.
    //
    // Midpoints are rounded toward zero. This is a stylistic choice: a
    // 1-logical-pixel line at 150% scale should render as 1 dp rather than
    // 2 dp.
    //
    // Pixel snapping is done in two phases:
    //
    //  1. Pre-layout metric snapping. Before Taffy computes layout, all
    //     authored absolute lengths are rounded in `to_taffy`. This
    //     includes borders, padding, gaps, and explicit sizes.
    //     Custom-measured leaf nodes have their measured sizes rounded up
    //     to integer device-pixel lengths.
    //
    //  2. Post-layout edge snapping. After Taffy resolves the tree, layout
    //     relationships such as flex shares, grid tracks, percentages, and
    //     centering can produce new fractional edge positions. Boxes now
    //     have edges in absolute coordinates, and snapping must decide
    //     where those edges land on the device-pixel grid.
    //
    // Ideally, post-layout snapping would satisfy:
    //
    //  - Edge closure. Two raw layout edges at the same absolute position
    //    should snap to the same pixel column.
    //  - Translation stability. A component's internal geometry should not
    //    change when it moves to a new absolute position.
    //
    // These goals are in tension because rounding is not associative.
    // The simple local schemes make different tradeoffs:
    //
    //  - Absolute edge rounding gives each window coordinate one answer,
    //    so coincident edges always close globally. But a span's snapped
    //    length is `round(far) - round(near)`, which may change by 1 dp
    //    as its absolute origin moves.
    //
    //  - Parent-relative edge rounding rounds each child inside its
    //    parent's coordinate space. This guarantees translation stability,
    //    but a shared edge reached through different parents can
    //    accumulate different rounding, causing non-closure between
    //    cousins.
    //
    //  - Length rounding rounds each width, height, and thickness
    //    independently and then places boxes from those rounded lengths.
    //    Sizes stay stable under translation, but neighboring boxes derive
    //    their shared boundary from different sources, so closure is not
    //    guaranteed.
    //
    // We apply absolute edge rounding for each element's outer box in
    // post-layout rounding to preserve closure. Border and padding widths
    // are not touched by post-layout rounding; they keep their pre-layout
    // rounded value so that they remain stable under translation.
    //
    // This gives both closure and translation stability in the case that
    // all local metrics are integer device-pixel lengths. Pre-layout
    // rounding covers that in most cases. The exception is metrics
    // resolved by layout relationships, such as percentages. Outer box
    // edges will still close globally, and painted border widths are still
    // snapped independently, but the raw content-box origin can carry a
    // 1dp residual into descendants.

    /// vue-native fork: whether the node is out of flow (`position:
    /// absolute`), which a scroll container measures without its padding.
    pub fn is_absolute(&self, id: LayoutId) -> bool {
        self.taffy
            .style(id.0)
            .is_ok_and(|style| style.position == taffy::style::Position::Absolute)
    }

    pub(crate) fn percentage_basis(&self, id: LayoutId, scale: f32) -> Option<Size<Pixels>> {
        use taffy::{style::CoreStyle as _, tree::DetailedLayoutInfo};
        // The bounds cache validates the slot generation and the current layout pass.
        self.absolute_layout_bounds.get(&id)?;
        let node = id.0;
        let child = self.taffy.style(node).ok()?;
        let mut parent = self.taffy.parent(node)?;
        let absolute = child.position.is_out_of_flow();
        if absolute {
            while !self
                .taffy
                .style(parent)
                .ok()?
                .is_containing_block()
                .for_position(child.position)
            {
                let Some(ancestor) = self.taffy.parent(parent) else {
                    break;
                };
                parent = ancestor;
            }
        }
        let layout = self.taffy.unrounded_layout(parent);
        let grid = match self.taffy.detailed_layout_info(parent) {
            DetailedLayoutInfo::Grid(grid) => Some(grid),
            _ => None,
        };
        let (w, h) = if absolute {
            let rtl = self.taffy.style(parent).ok()?.direction.is_rtl();
            let padding = TaffyRect {
                left: layout.border.left + if rtl { layout.scrollbar_size.width } else { 0. },
                right: layout.size.width
                    - layout.border.right
                    - if rtl { 0. } else { layout.scrollbar_size.width },
                top: layout.border.top,
                bottom: layout.size.height - layout.border.bottom - layout.scrollbar_size.height,
            };
            let area = if let Some(grid) = grid {
                grid.resolve_absolute_grid_area(
                    child.grid_row.clone(),
                    child.grid_column.clone(),
                    self.taffy.style(parent).ok()?.direction,
                    padding,
                )
            } else {
                padding
            };
            (area.right - area.left, area.bottom - area.top)
        } else if let Some(grid) = grid {
            // Taffy stores grid areas in the source order of its visible, in-flow children.
            let index = self
                .taffy
                .child_ids(parent)
                .filter(|&id| {
                    self.taffy.style(id).is_ok_and(|s| {
                        s.display != taffy::style::Display::None && !s.position.is_out_of_flow()
                    })
                })
                .position(|id| id == node)?;
            let (_, area) = grid.item_grid_area(index)?;
            (area.width, area.height)
        } else {
            (
                layout.content_box_width() - layout.scrollbar_size.width,
                layout.content_box_height() - layout.scrollbar_size.height,
            )
        };
        Some(crate::size(
            Pixels(w.max(0.) / scale),
            Pixels(h.max(0.) / scale),
        ))
    }

    pub fn layout_bounds(&mut self, id: LayoutId, scale_factor: f32) -> Bounds<Pixels> {
        if let Some(layout) = self.absolute_layout_bounds.get(&id).cloned() {
            return layout;
        }

        let layout = self.taffy.layout(id.into()).expect(EXPECT_MESSAGE);
        let layout_location = layout.location;
        let layout_size = layout.size;
        let parent = self.taffy.parent(id.0);

        let absolute_outer_origin = match parent {
            Some(parent_id) => {
                let parent_id = LayoutId::from(parent_id);
                self.layout_bounds(parent_id, scale_factor);
                let parent_origin = *self
                    .absolute_outer_origins
                    .get(&parent_id)
                    .expect("parent absolute outer origin should be cached");
                parent_origin + Point::from(layout_location)
            }
            None => Point::from(layout_location),
        };
        self.absolute_outer_origins
            .insert(id, absolute_outer_origin);

        let absolute_far = absolute_outer_origin + Point::from(Size::from(layout_size));
        let snapped_bounds = Bounds::from_corners(
            absolute_outer_origin.map(round_half_toward_zero),
            absolute_far.map(round_half_toward_zero),
        );

        let bounds = (snapped_bounds / scale_factor).map(Pixels);
        self.absolute_layout_bounds.insert(id, bounds);
        bounds
    }
}

/// A unique identifier for a layout node, generated when requesting a layout from Taffy
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
#[repr(transparent)]
pub struct LayoutId(NodeId);

impl LayoutId {
    fn to_taffy_slice(node_ids: &[Self]) -> &[taffy::NodeId] {
        // SAFETY: LayoutId is repr(transparent) to taffy::tree::NodeId.
        unsafe { std::mem::transmute::<&[LayoutId], &[taffy::NodeId]>(node_ids) }
    }
}

impl std::hash::Hash for LayoutId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        u64::from(self.0).hash(state);
    }
}

impl From<NodeId> for LayoutId {
    fn from(node_id: NodeId) -> Self {
        Self(node_id)
    }
}

impl From<LayoutId> for NodeId {
    fn from(layout_id: LayoutId) -> NodeId {
        layout_id.0
    }
}

fn snap_measured_size_to_device_pixels(size: Size<Pixels>, scale_factor: f32) -> Size<f32> {
    size.map(|d| ceil_to_device_pixel(d.0.max(0.0), scale_factor))
}

fn border_widths_to_taffy(
    widths: &Edges<AbsoluteLength>,
    rem_size: Pixels,
    scale_factor: f32,
) -> TaffyRect<taffy::style::LengthPercentage> {
    let snap = |w: &AbsoluteLength| {
        taffy::style::LengthPercentage::length(round_stroke_to_device_pixel(
            w.to_pixels(rem_size).0,
            scale_factor,
        ))
    };
    TaffyRect {
        top: snap(&widths.top),
        right: snap(&widths.right),
        bottom: snap(&widths.bottom),
        left: snap(&widths.left),
    }
}

trait ToTaffy<Output> {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> Output;
}

impl ToTaffy<taffy::style::Style> for Style {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> taffy::style::Style {
        use taffy::style_helpers::{fr, length, minmax, repeat};

        fn to_grid_line(
            placement: &Range<crate::GridPlacement>,
        ) -> taffy::Line<taffy::GridPlacement> {
            taffy::Line {
                start: placement.start.into(),
                end: placement.end.into(),
            }
        }

        fn to_grid_repeat<T: taffy::style::CheapCloneStr>(
            unit: &Option<GridTemplate>,
        ) -> Vec<taffy::GridTemplateComponent<T>> {
            unit.map(|template| {
                match template.min_size {
                    // grid-template-*: repeat(<number>, minmax(0, 1fr));
                    crate::GridTemplateMinSize::Zero => {
                        vec![repeat(
                            template.repeat,
                            vec![minmax(length(0.0_f32), fr(1.0_f32))],
                        )]
                    }
                    // grid-template-*: repeat(<number>, minmax(min-content, 1fr));
                    crate::GridTemplateMinSize::MinContent => {
                        vec![repeat(
                            template.repeat,
                            vec![minmax(min_content(), fr(1.0_f32))],
                        )]
                    }
                    // grid-template-*: repeat(<number>, minmax(0, max-content))
                    crate::GridTemplateMinSize::MaxContent => {
                        vec![repeat(
                            template.repeat,
                            vec![minmax(length(0.0_f32), max_content())],
                        )]
                    }
                }
            })
            .unwrap_or_default()
        }

        // An ARBITRARY track list (`1fr auto`, `minmax(0,1fr) 200px`), which
        // `to_grid_repeat`'s one-shape `repeat()` cannot express. Each track is
        // already in `minmax()` normal form, so this is a straight translation.
        fn to_track_size_min(size: crate::GridTrackSize) -> taffy::MinTrackSizingFunction {
            use crate::GridTrackSize as G;
            use taffy::MinTrackSizingFunction as M;
            match size {
                G::Auto => M::auto(),
                G::MinContent => M::min_content(),
                G::MaxContent => M::max_content(),
                G::Px(v) => M::length(v),
                G::Percent(v) => M::percent(v),
                // `<flex>` is not a valid track MINIMUM in CSS; it computes to
                // `auto` there, so it does here.
                G::Fr(_) => M::auto(),
            }
        }
        fn to_track_size_max(size: crate::GridTrackSize) -> taffy::MaxTrackSizingFunction {
            use crate::GridTrackSize as G;
            use taffy::MaxTrackSizingFunction as M;
            match size {
                G::Auto => M::auto(),
                G::MinContent => M::min_content(),
                G::MaxContent => M::max_content(),
                G::Px(v) => M::length(v),
                G::Percent(v) => M::percent(v),
                G::Fr(v) => M::fr(v),
            }
        }
        fn to_grid_tracks<T: taffy::style::CheapCloneStr>(
            tracks: &Option<Vec<crate::GridTrack>>,
            auto: Option<crate::GridAutoRepeat>,
        ) -> Option<Vec<taffy::GridTemplateComponent<T>>> {
            let track =
                |t: &crate::GridTrack| minmax(to_track_size_min(t.min), to_track_size_max(t.max));
            tracks.as_ref().map(|list| {
                // vue-native fork: one `repeat(auto-fill | auto-fit, …)` run,
                // which taffy expands against the container at layout time.
                let run = auto
                    .map(|a| a.start as usize..(a.start as usize + a.len as usize))
                    .filter(|run| !run.is_empty() && run.end <= list.len());
                let mut out = Vec::with_capacity(list.len());
                for (index, t) in list.iter().enumerate() {
                    match (&run, auto) {
                        (Some(run), Some(a)) if run.contains(&index) => {
                            if index == run.start {
                                let count = if a.fit {
                                    taffy::RepetitionCount::AutoFit
                                } else {
                                    taffy::RepetitionCount::AutoFill
                                };
                                out.push(repeat(
                                    count,
                                    list[run.clone()].iter().map(track).collect(),
                                ));
                            }
                        }
                        _ => out.push(taffy::GridTemplateComponent::Single(track(t))),
                    }
                }
                out
            })
        }

        taffy::style::Style {
            display: self.display.into(),
            overflow: self.overflow.into(),
            scrollbar_width: self.scrollbar_width.to_taffy(rem_size, scale_factor),
            position: self.position.into(),
            inset: self.inset.to_taffy(rem_size, scale_factor),
            size: self.size.to_taffy(rem_size, scale_factor),
            min_size: self.min_size.to_taffy(rem_size, scale_factor),
            max_size: self.max_size.to_taffy(rem_size, scale_factor),
            aspect_ratio: self
                .aspect_ratio
                .map(f32::abs)
                .filter(|ratio| ratio.is_finite() && *ratio > 0.0),
            margin: self.margin.to_taffy(rem_size, scale_factor),
            padding: self.padding.to_taffy(rem_size, scale_factor),
            border: border_widths_to_taffy(&self.border_widths, rem_size, scale_factor),
            align_items: self.align_items.map(Into::into).unwrap_or_default(),
            align_self: self.align_self.map(|x| x.into()),
            align_content: self.align_content.map(Into::into).unwrap_or_default(),
            justify_content: self.justify_content.map(Into::into).unwrap_or_default(),
            gap: self.gap.to_taffy(rem_size, scale_factor),
            flex_direction: self.flex_direction.into(),
            flex_wrap: self.flex_wrap.into(),
            flex_basis: self.flex_basis.to_taffy(rem_size, scale_factor),
            flex_grow: self.flex_grow,
            flex_shrink: self.flex_shrink,
            // An explicit track list wins over the `repeat(N, …)` shorthand.
            grid_template_rows: to_grid_tracks(&self.vn_grid_rows, self.vn_grid_rows_auto)
                .unwrap_or_else(|| to_grid_repeat(&self.grid_rows)),
            grid_template_columns: to_grid_tracks(&self.vn_grid_cols, self.vn_grid_cols_auto)
                .unwrap_or_else(|| to_grid_repeat(&self.grid_cols)),
            grid_auto_columns: self
                .vn_grid_auto_cols
                .as_ref()
                .map(|tracks| {
                    tracks
                        .iter()
                        .map(|track| {
                            minmax(to_track_size_min(track.min), to_track_size_max(track.max))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            grid_auto_rows: self
                .vn_grid_auto_rows
                .as_ref()
                .map(|tracks| {
                    tracks
                        .iter()
                        .map(|track| {
                            minmax(to_track_size_min(track.min), to_track_size_max(track.max))
                        })
                        .collect()
                })
                .unwrap_or_default(),
            grid_auto_flow: match self.vn_grid_auto_flow.unwrap_or_default() {
                crate::GridAutoFlow::Row => taffy::GridAutoFlow::Row,
                crate::GridAutoFlow::Column => taffy::GridAutoFlow::Column,
                crate::GridAutoFlow::RowDense => taffy::GridAutoFlow::RowDense,
                crate::GridAutoFlow::ColumnDense => taffy::GridAutoFlow::ColumnDense,
            },
            justify_items: self.vn_justify_items.map(Into::into).unwrap_or_default(),
            justify_self: self.vn_justify_self.map(|x| x.into()),
            grid_row: self
                .grid_location
                .as_ref()
                .map(|location| to_grid_line(&location.row))
                .unwrap_or_default(),
            grid_column: self
                .grid_location
                .as_ref()
                .map(|location| to_grid_line(&location.column))
                .unwrap_or_default(),
            contain: {
                let mut contain = taffy::Contain::NONE;
                if self.vn_contain_size & Style::CONTAIN_INLINE_SIZE != 0 {
                    contain |= taffy::Contain::INLINE_SIZE;
                }
                if self.vn_contain_size & Style::CONTAIN_BLOCK_SIZE != 0 {
                    contain |= taffy::Contain::BLOCK_SIZE;
                }
                contain
            },
            ..Default::default()
        }
    }
}

impl ToTaffy<f32> for AbsoluteLength {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> f32 {
        round_to_device_pixel(self.to_pixels(rem_size).0, scale_factor)
    }
}

impl ToTaffy<taffy::style::LengthPercentageAuto> for Length {
    fn to_taffy(
        &self,
        rem_size: Pixels,
        scale_factor: f32,
    ) -> taffy::prelude::LengthPercentageAuto {
        match self {
            Length::Definite(length) => length.to_taffy(rem_size, scale_factor),
            Length::Auto | Length::VnCalcSize(_) => taffy::prelude::LengthPercentageAuto::auto(),
        }
    }
}

impl ToTaffy<taffy::style::Dimension> for Length {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> taffy::prelude::Dimension {
        match self {
            Length::Definite(length) => length.to_taffy(rem_size, scale_factor),
            Length::Auto => taffy::prelude::Dimension::auto(),
            Length::VnCalcSize(c) => basis_dimension(c.basis),
        }
    }
}

/// vue-native fork: a node whose sizing properties hold a `calc-size()` other than its basis
/// alone, which [`TaffyLayoutEngine::compute_layout`] lays out twice. Per axis: size, min, max.
struct CalcSizeNode {
    id: LayoutId,
    width: [Option<crate::VnCalcSize>; 3],
    height: [Option<crate::VnCalcSize>; 3],
    flex_basis: Option<crate::VnCalcSize>,
}

impl CalcSizeNode {
    fn of(id: LayoutId, style: &Style) -> Option<Self> {
        let calc = |l: &Length| match l {
            Length::VnCalcSize(c) if !c.is_basis() => Some(*c),
            _ => None,
        };
        let node = CalcSizeNode {
            id,
            width: [
                calc(&style.size.width),
                calc(&style.min_size.width),
                calc(&style.max_size.width),
            ],
            height: [
                calc(&style.size.height),
                calc(&style.min_size.height),
                calc(&style.max_size.height),
            ],
            flex_basis: calc(&style.flex_basis),
        };
        (node.width.iter().chain(&node.height).any(Option::is_some) || node.flex_basis.is_some())
            .then_some(node)
    }
}

/// `size * scale + offset` with `size` the measured basis in logical pixels, as [`calc_handle`]
/// returns it: a percentage in the offset still resolves against the containing block.
fn calc_size_value(
    c: &crate::VnCalcSize,
    size: f32,
    rem_size: Pixels,
    scale_factor: f32,
) -> Result<*const (), f32> {
    let scaled = c.scale * size;
    let (fraction, pixels) = match c.offset {
        DefiniteLength::Absolute(a) => (0., scaled + a.to_pixels(rem_size).0),
        DefiniteLength::Fraction(f) => (f, scaled),
        DefiniteLength::Calc { fraction, pixels } => (fraction, scaled + pixels.0),
    };
    if fraction == 0. {
        return Err(round_to_device_pixel(pixels.max(0.), scale_factor));
    }
    calc_handle(fraction, Pixels(pixels), scale_factor)
}

/// vue-native fork: the keyword a `calc-size()` is measured from; also its whole layout when it
/// is the identity (`calc-size(auto, size)`).
fn basis_dimension(basis: crate::VnSizeBasis) -> taffy::style::Dimension {
    use crate::VnSizeBasis as B;
    use taffy::style::Dimension as D;
    match basis {
        B::Auto => D::auto(),
        B::MinContent => D::min_content(),
        B::MaxContent => D::max_content(),
        B::FitContent => D::fit_content(),
        B::Content => D::content(),
    }
}

impl ToTaffy<taffy::style::LengthPercentage> for DefiniteLength {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> taffy::style::LengthPercentage {
        match self {
            DefiniteLength::Absolute(length) => length.to_taffy(rem_size, scale_factor),
            DefiniteLength::Fraction(fraction) => {
                taffy::style::LengthPercentage::percent(*fraction)
            }
            DefiniteLength::Calc { fraction, pixels } => {
                match calc_handle(*fraction, *pixels, scale_factor) {
                    Ok(handle) => taffy::style::LengthPercentage::calc(handle),
                    Err(pixels) => taffy::style::LengthPercentage::length(pixels),
                }
            }
        }
    }
}

impl ToTaffy<taffy::style::LengthPercentageAuto> for DefiniteLength {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> taffy::style::LengthPercentageAuto {
        match self {
            DefiniteLength::Absolute(length) => length.to_taffy(rem_size, scale_factor),
            DefiniteLength::Fraction(fraction) => {
                taffy::style::LengthPercentageAuto::percent(*fraction)
            }
            DefiniteLength::Calc { fraction, pixels } => {
                match calc_handle(*fraction, *pixels, scale_factor) {
                    Ok(handle) => taffy::style::LengthPercentageAuto::calc(handle),
                    Err(pixels) => taffy::style::LengthPercentageAuto::length(pixels),
                }
            }
        }
    }
}

impl ToTaffy<taffy::style::Dimension> for DefiniteLength {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> taffy::style::Dimension {
        match self {
            DefiniteLength::Absolute(length) => length.to_taffy(rem_size, scale_factor),
            DefiniteLength::Fraction(fraction) => taffy::style::Dimension::percent(*fraction),
            DefiniteLength::Calc { fraction, pixels } => {
                match calc_handle(*fraction, *pixels, scale_factor) {
                    Ok(handle) => taffy::style::Dimension::calc(handle),
                    Err(pixels) => taffy::style::Dimension::length(pixels),
                }
            }
        }
    }
}

/// vue-native fork: the taffy `calc()` handle for [`DefiniteLength::Calc`].
///
/// taffy treats a calc value as an opaque, 8-byte-aligned handle and asks its
/// tree to resolve it against the percentage basis (`resolve_calc_value`). A
/// pointer to a heap node would need an owner and a lifetime; the linear form
/// `fraction * basis + pixels` fits in the handle itself, so it is packed
/// there: the length's `f32` bits in the high half, the fraction's in the low
/// half with its three lowest mantissa bits cleared for taffy's tag (a relative
/// error under 2^-20). vue-native's vendored taffy (`vendor/taffy`, patched
/// into the vue-native workspace) decodes exactly this in
/// `TaffyView::resolve_calc_value`; the two halves are a contract. Upstream
/// taffy resolves every calc to zero, which is what this degrades to in a
/// build against it (only vue-native ever writes a `Calc`).
///
/// `Err(pixels)` when there is nothing to pack: a zero fraction is a plain
/// length, and taffy reserves the all-zero handle. On a 32-bit target the
/// handle has no room for two `f32`s, so the percentage half is dropped there.
fn calc_handle(fraction: f32, pixels: Pixels, scale_factor: f32) -> Result<*const (), f32> {
    let length = round_to_device_pixel(pixels.0, scale_factor);
    if fraction == 0. {
        return Err(length);
    }
    #[cfg(target_pointer_width = "64")]
    {
        let bits = ((length.to_bits() as u64) << 32) | (fraction.to_bits() & !0b111) as u64;
        if bits == 0 {
            return Err(length);
        }
        Ok(bits as usize as *const ())
    }
    #[cfg(not(target_pointer_width = "64"))]
    {
        Err(length)
    }
}

/// vue-native fork: the inverse of [`calc_handle`], for the leaves taffy 0.15
/// lays out through `compute_leaf_layout` with a caller-supplied resolver. It
/// must decode exactly what the vendored taffy's
/// `TaffyView::resolve_calc_value` does for every other node.
#[inline(always)]
fn resolve_calc_handle(val: *const (), basis: f32) -> f32 {
    #[cfg(target_pointer_width = "64")]
    {
        let bits = val as usize as u64;
        let length = f32::from_bits((bits >> 32) as u32);
        let fraction = f32::from_bits(bits as u32 & !0b111);
        fraction * basis + length
    }
    #[cfg(not(target_pointer_width = "64"))]
    {
        let _ = (val, basis);
        0.0
    }
}

impl ToTaffy<taffy::style::LengthPercentage> for AbsoluteLength {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> taffy::style::LengthPercentage {
        taffy::style::LengthPercentage::length(self.to_taffy(rem_size, scale_factor))
    }
}

impl ToTaffy<taffy::style::LengthPercentageAuto> for AbsoluteLength {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> taffy::style::LengthPercentageAuto {
        taffy::style::LengthPercentageAuto::length(self.to_taffy(rem_size, scale_factor))
    }
}

impl ToTaffy<taffy::style::Dimension> for AbsoluteLength {
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> taffy::style::Dimension {
        taffy::style::Dimension::length(self.to_taffy(rem_size, scale_factor))
    }
}

impl<T, T2> From<TaffyPoint<T>> for Point<T2>
where
    T: Into<T2>,
    T2: Clone + Debug + Default + PartialEq,
{
    fn from(point: TaffyPoint<T>) -> Point<T2> {
        Point {
            x: point.x.into(),
            y: point.y.into(),
        }
    }
}

impl<T, T2> From<Point<T>> for TaffyPoint<T2>
where
    T: Into<T2> + Clone + Debug + Default + PartialEq,
{
    fn from(val: Point<T>) -> Self {
        TaffyPoint {
            x: val.x.into(),
            y: val.y.into(),
        }
    }
}

impl<T, U> ToTaffy<TaffySize<U>> for Size<T>
where
    T: ToTaffy<U> + Clone + Debug + Default + PartialEq,
{
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> TaffySize<U> {
        TaffySize {
            width: self.width.to_taffy(rem_size, scale_factor),
            height: self.height.to_taffy(rem_size, scale_factor),
        }
    }
}

impl<T, U> ToTaffy<TaffyRect<U>> for Edges<T>
where
    T: ToTaffy<U> + Clone + Debug + Default + PartialEq,
{
    fn to_taffy(&self, rem_size: Pixels, scale_factor: f32) -> TaffyRect<U> {
        TaffyRect {
            top: self.top.to_taffy(rem_size, scale_factor),
            right: self.right.to_taffy(rem_size, scale_factor),
            bottom: self.bottom.to_taffy(rem_size, scale_factor),
            left: self.left.to_taffy(rem_size, scale_factor),
        }
    }
}

impl<T, U> From<TaffySize<T>> for Size<U>
where
    T: Into<U>,
    U: Clone + Debug + Default + PartialEq,
{
    fn from(taffy_size: TaffySize<T>) -> Self {
        Size {
            width: taffy_size.width.into(),
            height: taffy_size.height.into(),
        }
    }
}

impl<T, U> From<Size<T>> for TaffySize<U>
where
    T: Into<U> + Clone + Debug + Default + PartialEq,
{
    fn from(size: Size<T>) -> Self {
        TaffySize {
            width: size.width.into(),
            height: size.height.into(),
        }
    }
}

/// The space available for an element to be laid out in
#[derive(Copy, Clone, Default, Debug, Eq, PartialEq)]
pub enum AvailableSpace {
    /// The amount of space available is the specified number of pixels
    Definite(Pixels),
    /// The amount of space available is indefinite and the node should be laid out under a min-content constraint
    #[default]
    MinContent,
    /// The amount of space available is indefinite and the node should be laid out under a max-content constraint
    MaxContent,
}

impl AvailableSpace {
    /// Returns a `Size` with both width and height set to `AvailableSpace::MinContent`.
    ///
    /// This function is useful when you want to create a `Size` with the minimum content constraints
    /// for both dimensions.
    ///
    /// # Examples
    ///
    /// ```
    /// use gpui::AvailableSpace;
    /// let min_content_size = AvailableSpace::min_size();
    /// assert_eq!(min_content_size.width, AvailableSpace::MinContent);
    /// assert_eq!(min_content_size.height, AvailableSpace::MinContent);
    /// ```
    pub const fn min_size() -> Size<Self> {
        Size {
            width: Self::MinContent,
            height: Self::MinContent,
        }
    }
}

impl From<AvailableSpace> for TaffyAvailableSpace {
    fn from(space: AvailableSpace) -> TaffyAvailableSpace {
        match space {
            AvailableSpace::Definite(Pixels(value)) => TaffyAvailableSpace::Definite(value),
            AvailableSpace::MinContent => TaffyAvailableSpace::MinContent,
            AvailableSpace::MaxContent => TaffyAvailableSpace::MaxContent,
        }
    }
}

impl From<TaffyAvailableSpace> for AvailableSpace {
    fn from(space: TaffyAvailableSpace) -> AvailableSpace {
        match space {
            TaffyAvailableSpace::Definite(value) => AvailableSpace::Definite(Pixels(value)),
            TaffyAvailableSpace::MinContent => AvailableSpace::MinContent,
            TaffyAvailableSpace::MaxContent => AvailableSpace::MaxContent,
        }
    }
}

impl From<Pixels> for AvailableSpace {
    fn from(pixels: Pixels) -> Self {
        AvailableSpace::Definite(pixels)
    }
}

impl From<Size<Pixels>> for Size<AvailableSpace> {
    fn from(size: Size<Pixels>) -> Self {
        Size {
            width: AvailableSpace::Definite(size.width),
            height: AvailableSpace::Definite(size.height),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentage_basis_uses_unscaled_content_and_padding_boxes() {
        use taffy::prelude::TaffyMaxContent;

        let mut engine = TaffyLayoutEngine::new();
        let mut make =
            |style, children: &[LayoutId]| engine.request_layout(style, Pixels(20.), 2., children);
        let child = make(Style::default(), &[]);
        let abs = make(
            Style {
                position: crate::Position::Absolute,
                ..Default::default()
            },
            &[],
        );
        let parent = make(
            Style {
                display: Display::Block,
                size: size(Pixels(160.).into(), Pixels(100.).into()),
                padding: Edges::all(crate::rems(1.).into()),
                border_widths: Edges::all(Pixels(3.).into()),
                ..Default::default()
            },
            &[child, abs],
        );
        engine
            .taffy
            .compute_layout(parent.into(), TaffySize::MAX_CONTENT)
            .unwrap();
        assert_eq!(engine.percentage_basis(child, 2.), None);
        engine.layout_bounds(child, 2.);
        engine.layout_bounds(abs, 2.);
        assert_eq!(
            engine.percentage_basis(child, 2.),
            Some(size(Pixels(114.), Pixels(54.)))
        );
        assert_eq!(
            engine.percentage_basis(abs, 2.),
            Some(size(Pixels(154.), Pixels(94.)))
        );
        assert_eq!(engine.percentage_basis(parent, 2.), None);
        engine.clear();
        assert_eq!(engine.percentage_basis(child, 2.), None);
    }

    #[test]
    fn flex_and_grid_lay_their_children_out_in_order_modified_document_order() {
        let mut engine = TaffyLayoutEngine::new();
        let leaf = |engine: &mut TaffyLayoutEngine, order: i32| {
            let style = Style {
                vn_order: order,
                ..Default::default()
            };
            engine.request_layout(style, Pixels(16.0), 1.0, &[])
        };
        let kids = [
            leaf(&mut engine, 2),
            leaf(&mut engine, -1),
            leaf(&mut engine, 0),
            leaf(&mut engine, 2),
        ];
        let laid_out = |engine: &mut TaffyLayoutEngine, display: Display| {
            let style = Style {
                display,
                ..Default::default()
            };
            let root = engine.request_layout(style, Pixels(16.0), 1.0, &kids);
            engine
                .taffy
                .child_ids(root.into())
                .map(LayoutId::from)
                .collect::<Vec<_>>()
        };
        let modified = vec![kids[1], kids[2], kids[0], kids[3]];
        assert_eq!(laid_out(&mut engine, Display::Flex), modified);
        assert_eq!(laid_out(&mut engine, Display::Grid), modified);
        assert_eq!(laid_out(&mut engine, Display::Block), kids.to_vec());
        engine.clear();
        let fresh = [leaf(&mut engine, 0), leaf(&mut engine, 0)];
        let root = engine.request_layout(
            Style {
                display: Display::Flex,
                ..Default::default()
            },
            Pixels(16.0),
            1.0,
            &fresh,
        );
        assert!(
            engine
                .taffy
                .child_ids(root.into())
                .map(LayoutId::from)
                .eq(fresh)
        );
    }

    #[test]
    fn implicit_fractional_columns_share_the_container_width() {
        let mut tree = TaffyTree::<()>::new();
        let children: Vec<_> = (0..3)
            .map(|_| tree.new_leaf(Default::default()).unwrap())
            .collect();
        let style = Style {
            display: crate::Display::Grid,
            size: Size {
                width: Length::Definite(Pixels(300.0).into()),
                height: Length::Definite(Pixels(40.0).into()),
            },
            vn_grid_auto_flow: Some(crate::GridAutoFlow::Column),
            vn_grid_auto_cols: Some(vec![crate::GridTrack::fr(1.0)]),
            ..Default::default()
        };
        let root = tree
            .new_with_children(style.to_taffy(Pixels(16.0), 1.0), &children)
            .unwrap();
        tree.compute_layout(
            root,
            TaffySize {
                width: TaffyAvailableSpace::Definite(300.0),
                height: TaffyAvailableSpace::Definite(40.0),
            },
        )
        .unwrap();
        for (index, child) in children.into_iter().enumerate() {
            let layout = tree.layout(child).unwrap();
            assert_eq!(layout.size.width, 100.0);
            assert_eq!(layout.location.x, index as f32 * 100.0);
        }
    }

    #[test]
    fn implicit_row_patterns_repeat_in_layout() {
        use crate::GridTrackSize as Track;
        let mut tree = TaffyTree::<()>::new();
        let children: Vec<_> = (0..5)
            .map(|_| tree.new_leaf(Default::default()).unwrap())
            .collect();
        let style = Style {
            display: crate::Display::Grid,
            vn_grid_cols: Some(vec![crate::GridTrack::fixed(Track::Px(100.0))]),
            vn_grid_auto_rows: Some(vec![
                crate::GridTrack::fixed(Track::Px(20.0)),
                crate::GridTrack::fixed(Track::Px(35.0)),
            ]),
            ..Default::default()
        };
        let root = tree
            .new_with_children(style.to_taffy(Pixels(16.0), 1.0), &children)
            .unwrap();
        tree.compute_layout(
            root,
            TaffySize {
                width: TaffyAvailableSpace::MaxContent,
                height: TaffyAvailableSpace::MaxContent,
            },
        )
        .unwrap();
        for ((height, top), child) in [
            (20.0, 0.0),
            (35.0, 20.0),
            (20.0, 55.0),
            (35.0, 75.0),
            (20.0, 110.0),
        ]
        .into_iter()
        .zip(children)
        {
            let layout = tree.layout(child).unwrap();
            assert_eq!(layout.size.height, height);
            assert_eq!(layout.location.y, top);
        }
    }

    #[test]
    fn implicit_auto_reset_replaces_a_fixed_row_pattern() {
        use crate::{GridTrack, GridTrackSize, Refineable, StyleRefinement};
        let mut style = Style {
            display: crate::Display::Grid,
            vn_grid_cols: Some(vec![GridTrack::fixed(GridTrackSize::Px(100.0))]),
            vn_grid_auto_rows: Some(vec![GridTrack::fixed(GridTrackSize::Px(50.0))]),
            ..Default::default()
        };
        style.refine(&StyleRefinement {
            vn_grid_auto_rows: Some(Vec::new()),
            ..Default::default()
        });
        let mut tree = TaffyTree::<()>::new();
        let children: Vec<_> = [17.0, 29.0]
            .into_iter()
            .map(|height| {
                let mut leaf_style = taffy::style::Style::default();
                leaf_style.size.height = taffy::style::Dimension::length(height);
                tree.new_leaf(leaf_style).unwrap()
            })
            .collect();
        let root = tree
            .new_with_children(style.to_taffy(Pixels(16.0), 1.0), &children)
            .unwrap();
        tree.compute_layout(
            root,
            TaffySize {
                width: TaffyAvailableSpace::MaxContent,
                height: TaffyAvailableSpace::MaxContent,
            },
        )
        .unwrap();
        assert_eq!(tree.layout(children[1]).unwrap().location.y, 17.0);
        assert_eq!(tree.layout(root).unwrap().size.height, 46.0);
    }

    #[test]
    fn border_widths_to_taffy_use_stroke_snapping() {
        let border_widths = Edges {
            top: Pixels(0.0).into(),
            right: Pixels(0.4).into(),
            bottom: Pixels(0.5).into(),
            left: Pixels(1.6).into(),
        };
        let taffy_border = border_widths_to_taffy(&border_widths, Pixels(16.0), 1.0);

        assert_eq!(
            taffy_border.top,
            taffy::style::LengthPercentage::length(0.0)
        );
        assert_eq!(
            taffy_border.right,
            taffy::style::LengthPercentage::length(1.0)
        );
        assert_eq!(
            taffy_border.bottom,
            taffy::style::LengthPercentage::length(1.0)
        );
        assert_eq!(
            taffy_border.left,
            taffy::style::LengthPercentage::length(2.0)
        );
    }
}
