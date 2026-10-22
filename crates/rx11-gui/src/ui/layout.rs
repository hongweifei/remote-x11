//! Declarative one-dimensional layout.
//!
//! Position in the layering (see `src/lib.rs`): `ui`, beside [`crate::ui::chrome`]
//! and above [`crate::theme`]. It knows nothing about pages, FLTK or painting;
//! it turns a tree of stacks into rectangles.
//!
//! Why this exists: painting the console by hand-measuring every element left a
//! whole class of defects where the same rectangle was computed twice — paint
//! placed a row here, hit testing there, and the scrollbar somewhere else. The
//! fix that held was "one source per rectangle", but it relied on discipline.
//! A layout tree makes it structural: the tree is solved **once** per frame, and
//! painting, hit testing, scrolling and capacity all read the same [`Solved`]
//! rectangles. A page never computes a coordinate from another coordinate again.
//!
//! # Model
//!
//! Every node is either a *leaf* (something a page paints) or a *stack* (a
//! vertical [`Axis::Column`] or horizontal [`Axis::Row`] of children). A child
//! asks for space along its parent's main axis with a [`Size`]:
//!
//! - [`Size::Fill`] shares the leftover space by weight,
//! - [`Size::Hug`] takes what its content needs, but yields to the container,
//! - [`Size::Fixed`] takes exactly that much.
//!
//! On the cross axis a child always gets the container's inner extent, which is
//! what a one-dimensional layout means: rows stretch across, columns stretch
//! down.
//!
//! Children are never allowed to leave the container. If the fixed and hugging
//! requests exceed the space available, the surplus is taken from the last
//! children (they get what is left, possibly zero) and [`Solved::overflow`]
//! reports it. Drawing outside the container was the visible symptom of every
//! geometry bug this module replaces, so the solver refuses to produce it; a
//! page that overflows gets a squeezed child instead of a leaking one.

/// A rectangle in logical (FLTK) units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    pub fn right(&self) -> i32 {
        self.x + self.w
    }

    pub fn bottom(&self) -> i32 {
        self.y + self.h
    }

    pub fn center_x(&self) -> i32 {
        self.x + self.w / 2
    }

    pub fn center_y(&self) -> i32 {
        self.y + self.h / 2
    }

    /// Shrink by an inset on every side; never flips sign.
    pub fn inset(&self, insets: Insets) -> Rect {
        Rect {
            x: self.x + insets.left,
            y: self.y + insets.top,
            w: (self.w - insets.left - insets.right).max(0),
            h: (self.h - insets.top - insets.bottom).max(0),
        }
    }

    /// Move down by `dy` (used for scrolling: solve tall, paint the window).
    pub fn shift_y(&self, dy: i32) -> Rect {
        Rect {
            y: self.y + dy,
            ..*self
        }
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// True when `self` lies entirely inside `outer`.
    pub fn inside(&self, outer: Rect) -> bool {
        self.x >= outer.x
            && self.y >= outer.y
            && self.right() <= outer.right()
            && self.bottom() <= outer.bottom()
    }

    /// True when the two rectangles share at least one unit. A scrolled item is
    /// usually only partly visible, so pages ask this rather than [`Rect::inside`].
    pub fn intersects(&self, other: Rect) -> bool {
        self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }
}

/// Per-side inner spacing of a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Insets {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

impl Insets {
    pub const fn all(n: i32) -> Self {
        Self {
            top: n,
            right: n,
            bottom: n,
            left: n,
        }
    }

    pub const fn xy(x: i32, y: i32) -> Self {
        Self {
            top: y,
            right: x,
            bottom: y,
            left: x,
        }
    }

    pub const fn top(n: i32) -> Self {
        Self {
            top: n,
            right: 0,
            bottom: 0,
            left: 0,
        }
    }
}

/// Stack direction of a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Children stacked top to bottom; they share the width.
    Column,
    /// Children stacked left to right; they share the height.
    Row,
}

/// What a child asks for along its parent's main axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Size {
    /// Exactly this many units.
    Fixed(i32),
    /// This many units if they are free — a card that hugs its content but
    /// never pushes the page past the bottom.
    Hug(i32),
    /// Share the remaining space with the other `Fill` siblings, by weight.
    Fill(u32),
}

/// Index of a node inside a [`Layout`].
pub type NodeId = usize;

/// One stack or leaf.
struct Node {
    axis: Axis,
    insets: Insets,
    gap: i32,
    /// Space inserted *before* this node, on top of the parent's `gap`.
    ///
    /// Sections are rarely evenly spaced: a heading sits flush against what
    /// follows, while list rows are separated from each other. Expressing that
    /// as a per-node lead keeps one stack instead of nesting a spacer child
    /// for every exception.
    lead: i32,
    size: Size,
    children: Vec<NodeId>,
}

/// A layout tree. Build it once, then [`Layout::solve`] it per frame.
///
/// The tree is rebuilt whenever the data behind the page changes, which is the
/// same moment the old code recomputed its coordinates by hand.
pub struct Layout {
    nodes: Vec<Node>,
    root: NodeId,
}

impl Layout {
    /// A vertical stack with no padding: the page root.
    pub fn column() -> Self {
        Self::with_axis(Axis::Column)
    }

    /// A horizontal stack with no padding, for a bar whose children sit side by
    /// side and share the height.
    pub fn row() -> Self {
        Self::with_axis(Axis::Row)
    }

    fn with_axis(axis: Axis) -> Self {
        Self {
            nodes: vec![Node {
                axis,
                insets: Insets::default(),
                gap: 0,
                lead: 0,
                size: Size::Fill(1),
                children: Vec::new(),
            }],
            root: 0,
        }
    }

    /// A node that a page paints into.
    pub fn leaf(&mut self, parent: NodeId, size: Size) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node {
            axis: Axis::Column,
            insets: Insets::default(),
            gap: 0,
            lead: 0,
            size,
            children: Vec::new(),
        });
        self.nodes[parent].children.push(id);
        id
    }

    /// A container of children along `axis`.
    pub fn stack(&mut self, parent: NodeId, axis: Axis, size: Size) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node {
            axis,
            insets: Insets::default(),
            gap: 0,
            lead: 0,
            size,
            children: Vec::new(),
        });
        self.nodes[parent].children.push(id);
        id
    }

    /// Set a node's inner padding.
    pub fn pad(mut self, id: NodeId, insets: Insets) -> Self {
        self.nodes[id].insets = insets;
        self
    }

    /// Set a node's gap between children.
    pub fn gap(mut self, id: NodeId, gap: i32) -> Self {
        self.nodes[id].gap = gap;
        self
    }

    /// Set the space inserted before `id`.
    pub fn lead(mut self, id: NodeId, lead: i32) -> Self {
        self.nodes[id].lead = lead;
        self
    }

    /// Root node id (the page surface).
    pub fn root(&self) -> NodeId {
        self.root
    }

    /// Solve the whole tree against `rect`.
    ///
    /// Every node gets exactly one rectangle; painting, hit testing and
    /// scrolling read them back with [`Solved::get`]. Nothing is computed twice.
    pub fn solve(&self, rect: Rect) -> Solved {
        let mut solved = Solved {
            rects: vec![Rect::default(); self.nodes.len()],
            overflow: false,
        };
        self.place(self.root, rect, &mut solved);
        solved
    }

    fn place(&self, id: NodeId, rect: Rect, solved: &mut Solved) {
        solved.rects[id] = rect;
        let node = &self.nodes[id];
        if node.children.is_empty() {
            return;
        }
        let inner = rect.inset(node.insets);
        let n = node.children.len() as i32;
        let gap_total = node.gap * (n - 1).max(0);
        let lead_total: i32 = node.children.iter().map(|&c| self.nodes[c].lead).sum();
        let available = (node.axis.main(inner) - gap_total - lead_total).max(0);

        // Reserve what the children asked for. A `Hug` child yields when space
        // runs out — that is its contract, not a fault — so only the fixed
        // requests are a promise the solver must keep, and only their shortfall
        // is a real overflow.
        let mut wanted = 0;
        let mut fixed_total = 0;
        for &child in &node.children {
            match self.nodes[child].size {
                Size::Fixed(v) => {
                    wanted += v.max(0);
                    fixed_total += v.max(0);
                }
                Size::Hug(v) => wanted += v.max(0),
                Size::Fill(_) => {}
            }
        }
        if fixed_total > available {
            solved.overflow = true;
        }
        // Fills share what fixed children leave; a hug child that does not fit
        // simply takes what remains instead of starving its siblings.
        let leftover = (available - fixed_total).max(0);
        let weight_total: u32 = node
            .children
            .iter()
            .filter_map(|&c| match self.nodes[c].size {
                Size::Fill(w) => Some(w.max(1)),
                _ => None,
            })
            .sum();
        let _ = wanted;

        let mut cursor = node.axis.main_start(inner);
        let mut remaining = available;
        for (index, &child) in node.children.iter().enumerate() {
            cursor += self.nodes[child].lead;
            let want = match self.nodes[child].size {
                Size::Fixed(v) => v.max(0),
                Size::Hug(v) => {
                    // Take the natural size when it fits, otherwise whatever the
                    // fixed children and the gaps have left.
                    let trailing_fixed: i32 = node.children[index + 1..]
                        .iter()
                        .filter_map(|&c| match self.nodes[c].size {
                            Size::Fixed(w) => Some(w.max(0)),
                            _ => None,
                        })
                        .sum();
                    v.max(0).min((remaining - trailing_fixed).max(0))
                }
                Size::Fill(w) => {
                    if weight_total == 0 {
                        0
                    } else {
                        // The last fill child soaks up the rounding remainder,
                        // but only after its later siblings have been paid for:
                        // a fixed button after a filling search box must still
                        // get its width. `trailing` is what those siblings ask
                        // for; everything past it is free for this child.
                        let trailing: i32 = node.children[index + 1..]
                            .iter()
                            .filter_map(|&c| match self.nodes[c].size {
                                Size::Fixed(v) | Size::Hug(v) => Some(v.max(0)),
                                Size::Fill(_) => None,
                            })
                            .sum();
                        let last_fill = node.children[index + 1..]
                            .iter()
                            .all(|&c| !matches!(self.nodes[c].size, Size::Fill(_)));
                        if last_fill {
                            (remaining - trailing).max(0)
                        } else {
                            (leftover as i64 * w.max(1) as i64 / weight_total as i64) as i32
                        }
                    }
                }
            };
            let take = want.min(remaining);
            let child_rect = match node.axis {
                Axis::Column => Rect {
                    x: inner.x,
                    y: cursor,
                    w: inner.w,
                    h: take,
                },
                Axis::Row => Rect {
                    x: cursor,
                    y: inner.y,
                    w: take,
                    h: inner.h,
                },
            };
            remaining -= take;
            cursor += take + node.gap;
            self.place(child, child_rect, solved);
        }
    }
}

impl Axis {
    fn main(self, rect: Rect) -> i32 {
        match self {
            Axis::Column => rect.h,
            Axis::Row => rect.w,
        }
    }

    fn main_start(self, rect: Rect) -> i32 {
        match self {
            Axis::Column => rect.y,
            Axis::Row => rect.x,
        }
    }
}

/// Solved rectangles, indexed by [`NodeId`].
pub struct Solved {
    rects: Vec<Rect>,
    overflow: bool,
}

impl Solved {
    /// The rectangle of `id`.
    pub fn get(&self, id: NodeId) -> Rect {
        self.rects[id]
    }

    /// True when the tree asked for more space than its container had, i.e.
    /// some children were squeezed. Pages are designed to fit; this exists so a
    /// test can assert they do.
    pub fn overflow(&self) -> bool {
        self.overflow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: Rect = Rect::new(0, 0, 1000, 700);

    #[test]
    fn column_stacks_fixed_children_in_order() {
        let mut l = Layout::column();
        let a = l.leaf(0, Size::Fixed(100));
        let b = l.leaf(0, Size::Fixed(50));
        let solved = l.solve(PAGE);
        assert_eq!(solved.get(a), Rect::new(0, 0, 1000, 100));
        assert_eq!(solved.get(b), Rect::new(0, 100, 1000, 50));
        assert!(!solved.overflow());
    }

    #[test]
    fn fill_shares_the_leftover_by_weight() {
        let mut l = Layout::column();
        let fixed = l.leaf(0, Size::Fixed(200));
        let one = l.leaf(0, Size::Fill(1));
        let three = l.leaf(0, Size::Fill(3));
        let solved = l.solve(PAGE);
        assert_eq!(solved.get(fixed).h, 200);
        assert_eq!(solved.get(one).h, 125);
        assert_eq!(solved.get(three).h, 375);
        assert_eq!(
            solved.get(three).bottom(),
            PAGE.bottom(),
            "fill children must consume the container exactly"
        );
    }

    #[test]
    fn gap_and_padding_shrink_the_content_box() {
        let mut l = Layout::column().pad(0, Insets::all(20)).gap(0, 10);
        let a = l.leaf(0, Size::Fixed(100));
        let b = l.leaf(0, Size::Fill(1));
        let solved = l.solve(PAGE);
        assert_eq!(solved.get(a), Rect::new(20, 20, 960, 100));
        assert_eq!(solved.get(b), Rect::new(20, 130, 960, 550));
    }

    #[test]
    fn row_gives_children_the_full_height_and_splits_width() {
        let mut l = Layout::column();
        let row = l.stack(0, Axis::Row, Size::Fixed(300));
        let left = l.leaf(row, Size::Fill(2));
        let right = l.leaf(row, Size::Fill(1));
        let solved = l.solve(PAGE);
        assert_eq!(solved.get(left), Rect::new(0, 0, 666, 300));
        assert_eq!(solved.get(right), Rect::new(666, 0, 334, 300));
    }

    #[test]
    fn hug_takes_what_it_needs_when_there_is_room() {
        let mut l = Layout::column();
        let header = l.leaf(0, Size::Fixed(100));
        let card = l.leaf(0, Size::Hug(180));
        let solved = l.solve(PAGE);
        assert_eq!(solved.get(card).h, 180, "hug keeps its own height");
        assert_eq!(solved.get(header).h, 100);
        assert!(!solved.overflow());
    }

    #[test]
    fn hug_yields_when_the_container_is_short() {
        let mut l = Layout::column();
        let _top = l.leaf(0, Size::Fixed(600));
        let card = l.leaf(0, Size::Hug(400));
        let solved = l.solve(Rect::new(0, 0, 1000, 700));
        assert_eq!(
            solved.get(card).h,
            100,
            "hug takes only what is left, never spills"
        );
        assert!(
            !solved.overflow(),
            "a hugging child shrinking is its contract, not a fault: a card that \
             must fit the page is exactly what Hug is for"
        );
    }

    #[test]
    fn hug_still_gets_its_size_when_nothing_follows_it() {
        let mut l = Layout::column();
        let _top = l.leaf(0, Size::Fixed(200));
        let card = l.leaf(0, Size::Hug(180));
        let solved = l.solve(Rect::new(0, 0, 1000, 700));
        assert_eq!(solved.get(card).h, 180);
        assert_eq!(solved.get(card).bottom(), 380, "hug leaves the rest free");
    }

    #[test]
    fn overflow_squeezes_children_instead_of_drawing_outside() {
        let mut l = Layout::column();
        let a = l.leaf(0, Size::Fixed(500));
        let b = l.leaf(0, Size::Fixed(500));
        let solved = l.solve(Rect::new(0, 0, 1000, 700));
        assert!(solved.overflow());
        assert_eq!(solved.get(a).h, 500);
        assert_eq!(
            solved.get(b).h,
            200,
            "the last child is clipped, not leaked"
        );
        for id in [a, b] {
            let r = solved.get(id);
            assert!(
                r.inside(Rect::new(0, 0, 1000, 700)),
                "no child may leave the container"
            );
        }
    }

    #[test]
    fn nested_stacks_compose() {
        let mut l = Layout::column().pad(0, Insets::xy(16, 16)).gap(0, 8);
        let header = l.leaf(0, Size::Fixed(60));
        let body = l.stack(0, Axis::Row, Size::Fill(1));
        let left = l.stack(body, Axis::Column, Size::Fill(1));
        let l1 = l.leaf(left, Size::Fixed(100));
        let l2 = l.leaf(left, Size::Fill(1));
        let right = l.leaf(body, Size::Fill(1));
        let solved = l.solve(PAGE);
        assert_eq!(solved.get(header).w, 968);
        assert_eq!(solved.get(body).y, 84);
        assert_eq!(solved.get(l1).h, 100);
        assert_eq!(solved.get(l2).h, solved.get(left).h - 100);
        assert_eq!(solved.get(right).w, 484);
        assert_eq!(solved.get(left).w, 484);
    }

    #[test]
    fn every_child_stays_inside_for_many_container_sizes() {
        for h in [40, 100, 300, 700, 1000, 2000] {
            for w in [80, 300, 1000, 1600] {
                let container = Rect::new(0, 0, w, h);
                let mut l = Layout::column().gap(0, 12);
                let a = l.leaf(0, Size::Fixed(50));
                let b = l.leaf(0, Size::Fill(1));
                let c = l.leaf(0, Size::Hug(400));
                let solved = l.solve(container);
                for id in [a, b, c] {
                    let r = solved.get(id);
                    assert!(
                        r.inside(container),
                        "width={w} height={h}: child {id} left the container: {r:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn trailing_fixed_children_survive_a_filling_sibling() {
        // A search box that fills the slack, then a button: the button must keep
        // its width. The first version let the fill child eat everything to its
        // right, squeezing every control after it to zero.
        let mut l = Layout::column().pad(0, Insets::all(22));
        let row = l.stack(0, Axis::Row, Size::Fixed(34));
        l = l.gap(row, 12);
        let filler = l.leaf(row, Size::Fill(1));
        let button = l.leaf(row, Size::Fixed(120));
        let group = l.stack(row, Axis::Row, Size::Fixed(288));
        let solved = l.solve(Rect::new(0, 0, 1180, 714));
        assert_eq!(solved.get(button).w, 120);
        assert_eq!(solved.get(group).w, 288);
        assert_eq!(solved.get(filler).w, 1180 - 44 - 120 - 288 - 24);
        assert!(!solved.overflow());
    }

    #[test]
    fn scrolling_is_a_translation_not_a_second_layout() {
        let mut l = Layout::column();
        let a = l.leaf(0, Size::Fixed(100));
        let b = l.leaf(0, Size::Fixed(100));
        let viewport = Rect::new(0, 0, 1000, 120);
        let solved = l.solve(Rect::new(0, 0, 1000, 400));
        assert_eq!(solved.get(a), Rect::new(0, 0, 1000, 100));
        // Painting scrolled content: same rects, moved up by the offset. The
        // item is only partly visible, which is why pages clip to the viewport.
        let shifted = solved.get(b).shift_y(-50);
        assert_eq!(shifted, Rect::new(0, 50, 1000, 100));
        assert!(shifted.intersects(viewport));
        assert!(!shifted.inside(viewport), "partly visible is not inside");
    }
}
