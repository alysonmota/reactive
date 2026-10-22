//! ui_overview.rs — one-file, runnable overview of the UI architecture on top of `reactive`.
//!
//! Put it at `examples/ui_overview.rs` of the `reactive` crate and run:
//!     cargo run --profile fast-dev --example ui_overview
//! and the incremental-layout fuzz test (400 random interactions vs. a full recompute):
//!     cargo test --profile fast-dev --example ui_overview
//!
//! Dependencies: `reactive` and `taffy` (flexbox only):
//!     taffy = { version = "0.15", default-features = false, features = ["std", "flexbox", "taffy_tree"] }
//! Everything platform-specific is replaced by a minimal, clearly marked stand-in so the
//! *data flow* is real and the backends are swappable:
//!
//!   real project                         this file
//!   ─────────────────────────────────    ─────────────────────────────────────────
//!   wayland-client + calloop             `Host::iteration` (one calloop dispatch)
//!   Ping/eventfd woken by flush hook     `wake: Rc<Cell<bool>>` set by the hook
//!   wl_surface.frame callback            "frame" step at the end of `iteration`
//!   Taffy                                real Taffy: `Tree::taffy` mirrors the UI tree
//!   cosmic-text                          text size = chars().count() x 1 cell (Taffy node context)
//!   tiny-skia / Vulkan                   `Renderer` trait + `AsciiRenderer`
//!   pixels                               character cells
//!
//! ───────────────────────────── data flow ─────────────────────────────
//!
//!   input event ─► handler (host context) ─► Sig::set            (batch: no effect runs)
//!                                               │ marks observers, enqueues effects
//!                                               ▼
//!                         flush hook ─► wake flag ─► Runtime::flush
//!                                               │ effects run: ONLY the bindings whose
//!                                               ▼ sources changed; they write to the Tree
//!                    Tree setters compare, then set dirty bits (LAYOUT) / damage rects (PAINT)
//!                                               │
//!                                               ▼ frame callback
//!   Taffy (if LAYOUT; cached) ─► sync ONLY dirty paths ─► paint damage ─► Renderer ─► present
//!
//! ───────────────────────────── rules ─────────────────────────────
//!  1. The UI tree is NOT reactive. Signals hold app state; effects are the only bridge.
//!  2. Components run ONCE. Dynamic parts are closures bound through effects (`bind`).
//!  3. Memos are pure and cannot create nodes → node creation happens only in the
//!     component body, in effects (`show`/`for_each`), or in the host.
//!  4. Lifetime = ownership. A re-run effect disposes what its previous run created, so
//!     `show` needs only `on_cleanup(remove_subtree)`. Keyed rows get their own owners.
//!  5. `set` never runs effects, so event handlers can never re-enter the tree.
//!  6. Never move a `Cx` (it holds a strong `Runtime`) into a stored `'static` closure:
//!     that creates a runtime → closure → runtime cycle. Builders clone `cx.tree` only.
//!  7. Every UI node owns a Taffy node, and structure/style/text changes are mirrored into it
//!     by the `Tree` setters (never by user code). Taffy caches per node, so a relayout only
//!     recomputes the dirty path. `Slot` nodes are flex wrappers that become `Display::None`
//!     while empty, so an empty `show` adds neither size nor gap.
//!  8. Layout is incremental end to end. Setters mark the node and its ancestors dirty
//!     (`Node::dirty`, same propagation as Taffy's cache). After Taffy recomputes, only the
//!     dirty path is visited; clean children are compared with their stored placement and
//!     skipped. Nodes store PARENT-RELATIVE rects, so moving a clean subtree costs nothing:
//!     absolute positions are derived top-down in paint/hit-test, and by walking up
//!     (`abs_rect`) when a single node needs damage. Taffy's own rounding pass walks the
//!     whole tree, so it is disabled and edges are rounded per node instead.

#![allow(dead_code)]

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    hash::Hash,
    rc::Rc,
};

use reactive::{FlushOptions, OwnerHandle, Runtime, RuntimeError, Signal};
use taffy::{
    AlignItems, AvailableSpace, Dimension, Display, FlexDirection, LengthPercentage,
    NodeId as TNode, Style, TaffyTree, compute_leaf_layout,
};

// ════════════════════════════════════════════════════════════════════════════
// 1. Ergonomic reactive layer
//    The core stays explicit (`try_get_copy(&rt)`). The UI layer installs the runtime
//    in a thread-local so user code can write `count.get()` / `count.set(1)`.
//    In release builds panic=abort, so `ok()` turning errors into panics is acceptable;
//    in debug it gives a readable message.
// ════════════════════════════════════════════════════════════════════════════

thread_local! {
    static RT: RefCell<Option<Runtime>> = const { RefCell::new(None) };
}

fn with_rt<R>(f: impl FnOnce(&Runtime) -> R) -> R {
    RT.with(|c| f(c.borrow().as_ref().expect("UI runtime not installed")))
}

fn ok<T>(r: Result<T, RuntimeError>) -> T {
    r.unwrap_or_else(|e| panic!("reactive: {e}"))
}

/// `Copy` signal handle that resolves the runtime through the thread-local.
struct Sig<T: 'static>(Signal<T>);

impl<T: 'static> Clone for Sig<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T: 'static> Copy for Sig<T> {}

impl<T: PartialEq + 'static> Sig<T> {
    /// Must be called with a current owner (inside `with_owner` or an effect scope).
    fn new(v: T) -> Self {
        Sig(with_rt(|rt| ok(rt.signal(v))))
    }
    fn set(self, v: T) {
        with_rt(|rt| ok(self.0.try_set(rt, v)))
    }
}

impl<T: 'static> Sig<T> {
    /// Tracked read (creates an edge when called inside an effect/memo).
    fn get(self) -> T
    where
        T: Copy,
    {
        with_rt(|rt| ok(self.0.try_get_copy(rt)))
    }
    /// Untracked read.
    fn peek(self) -> T
    where
        T: Copy,
    {
        with_rt(|rt| ok(self.0.try_peek_copy(rt)))
    }
    fn get_cloned(self) -> T
    where
        T: Clone,
    {
        with_rt(|rt| ok(self.0.try_get_cloned(rt)))
    }
    fn update(self, f: impl FnOnce(&mut T))
    where
        T: Clone,
    {
        with_rt(|rt| ok(self.0.try_update_cloned(rt, f)))
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 2. ui-core: geometry, styles, generational node tree, dirty tracking
// ════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
    fn at(self, ox: i32, oy: i32) -> Rect {
        Rect {
            x: self.x + ox,
            y: self.y + oy,
            ..self
        }
    }
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
    fn intersects(&self, o: &Rect) -> bool {
        !self.is_empty()
            && !o.is_empty()
            && self.x < o.x + o.w
            && o.x < self.x + self.w
            && self.y < o.y + o.h
            && o.y < self.y + self.h
    }
    fn intersect(self, o: Rect) -> Rect {
        let (x0, y0) = (self.x.max(o.x), self.y.max(o.y));
        let (x1, y1) = (
            (self.x + self.w).min(o.x + o.w),
            (self.y + self.h).min(o.y + o.h),
        );
        Rect {
            x: x0,
            y: y0,
            w: (x1 - x0).max(0),
            h: (y1 - y0).max(0),
        }
    }
    fn union(self, o: Rect) -> Rect {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return self;
        }
        let (x0, y0) = (self.x.min(o.x), self.y.min(o.y));
        let (x1, y1) = (
            (self.x + self.w).max(o.x + o.w),
            (self.y + self.h).max(o.y + o.h),
        );
        Rect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Default, Debug)]
enum Dir {
    #[default]
    Col,
    Row,
}

/// Properties that can change geometry → setting one dirties LAYOUT.
#[derive(Clone, PartialEq, Default)]
struct LayoutStyle {
    dir: Dir,
    gap: i32,
    pad: i32,
    border: bool, // occupies one cell on each side, like padding
    width: Option<i32>,
    height: Option<i32>,
    grow: bool, // takes a share of leftover main-axis space
}

/// Properties that only change pixels → setting one only adds a damage rect.
#[derive(Clone, PartialEq, Default)]
struct PaintStyle {
    fill: Option<char>,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Box,
    Text,
    /// Flex wrapper whose children are replaced dynamically (`show`, `for_each`).
    /// `Display::None` while empty → no size and no gap in the parent.
    Slot,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct NodeId {
    idx: u32,
    generation: u32, // stale ids (disposed nodes) never alias a reused slot
}

struct Node {
    kind: Kind,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    layout: LayoutStyle,
    paint: PaintStyle,
    text: String,
    rel: Rect,   // parent-relative placement, as of the last sync (= last painted)
    dirty: bool, // layout may have changed here or below; invariant: parent is dirty too
    tn: TNode,   // the mirrored Taffy node
    on_click: Option<Rc<dyn Fn()>>,
}

struct Slot {
    generation: u32,
    node: Option<Node>,
}

struct Tree {
    slots: Vec<Slot>,
    free: Vec<u32>,
    root: Option<NodeId>,
    dirty_layout: bool,
    damage: Option<Rect>, // union of regions that need repaint
    on_tick: Option<Rc<dyn Fn()>>,
    taffy: TaffyTree<usize>, // node context = number of text cells
    viewport: (i32, i32),    // the root fills it
}

/// UI style → Taffy style. Only called by `Tree` (rule 7).
fn to_style(kind: Kind, ls: &LayoutStyle, empty_slot: bool, fill: Option<(i32, i32)>) -> Style {
    let len = |v: i32| LengthPercentage::length(v as f32);
    let dim = |v: Option<i32>| v.map_or(Dimension::auto(), |v| Dimension::length(v as f32));
    let (fw, fh) = fill.map_or((None, None), |(w, h)| (Some(w), Some(h)));
    Style {
        display: if kind == Kind::Slot && empty_slot {
            Display::None
        } else {
            Display::Flex
        },
        flex_direction: if ls.dir == Dir::Row {
            FlexDirection::Row
        } else {
            FlexDirection::Column
        },
        // Children keep their own cross size (the old behavior); Taffy's default is stretch.
        align_items: AlignItems::FLEX_START,
        gap: taffy::Size {
            width: len(ls.gap),
            height: len(ls.gap),
        },
        padding: taffy::Rect {
            left: len(ls.pad),
            right: len(ls.pad),
            top: len(ls.pad),
            bottom: len(ls.pad),
        },
        border: {
            let b = len(ls.border as i32);
            taffy::Rect {
                left: b,
                right: b,
                top: b,
                bottom: b,
            }
        },
        size: taffy::Size {
            width: dim(ls.width.or(fw)),
            height: dim(ls.height.or(fh)),
        },
        flex_grow: if ls.grow { 1.0 } else { 0.0 },
        flex_shrink: 0.0, // cells don't shrink
        ..Style::default()
    }
}

impl Tree {
    fn new() -> Self {
        let mut taffy = TaffyTree::new();
        // Taffy's rounding pass visits the whole tree on every compute, so it is off;
        // `read_rel` snaps each node's edges to cells itself (O(visited)).
        taffy.disable_rounding();
        Tree {
            slots: vec![],
            free: vec![],
            root: None,
            dirty_layout: false,
            damage: None,
            on_tick: None,
            taffy,
            viewport: (0, 0),
        }
    }

    /// Recompute a node's Taffy style from the UI state; no-op (and no dirtying) if equal.
    fn restyle(&mut self, id: NodeId) {
        let Some(n) = self.node(id) else { return };
        let fill = (self.root == Some(id)).then_some(self.viewport);
        let style = to_style(
            n.kind,
            &n.layout,
            n.kind == Kind::Slot && n.children.is_empty(),
            fill,
        );
        let tn = n.tn;
        if self.taffy.style(tn).map_or(true, |s| *s != style) {
            self.taffy.set_style(tn, style).expect("taffy set_style");
        }
    }

    // ── storage ────────────────────────────────────────────────────────────
    fn create(&mut self, kind: Kind) -> NodeId {
        let style = to_style(kind, &LayoutStyle::default(), true, None);
        let tn = match kind {
            Kind::Text => self.taffy.new_leaf_with_context(style, 0),
            Kind::Box | Kind::Slot => self.taffy.new_leaf(style),
        }
        .expect("taffy node");
        let node = Node {
            kind,
            parent: None,
            children: vec![],
            layout: LayoutStyle::default(),
            paint: PaintStyle::default(),
            text: String::new(),
            rel: Rect::default(),
            dirty: true, // never placed yet
            tn,
            on_click: None,
        };
        if let Some(idx) = self.free.pop() {
            let s = &mut self.slots[idx as usize];
            s.node = Some(node);
            NodeId {
                idx,
                generation: s.generation,
            }
        } else {
            self.slots.push(Slot {
                generation: 0,
                node: Some(node),
            });
            NodeId {
                idx: (self.slots.len() - 1) as u32,
                generation: 0,
            }
        }
    }
    fn node(&self, id: NodeId) -> Option<&Node> {
        let s = self.slots.get(id.idx as usize)?;
        if s.generation == id.generation {
            s.node.as_ref()
        } else {
            None
        }
    }
    fn node_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        let s = self.slots.get_mut(id.idx as usize)?;
        if s.generation == id.generation {
            s.node.as_mut()
        } else {
            None
        }
    }
    fn live_nodes(&self) -> usize {
        self.slots.iter().filter(|s| s.node.is_some()).count()
    }
    fn set_root(&mut self, id: NodeId) {
        self.root = Some(id);
        self.restyle(id); // the root fills the viewport
        self.mark_dirty(id);
    }

    // ── structure ──────────────────────────────────────────────────────────
    fn append(&mut self, parent: NodeId, child: NodeId) {
        if self.node(parent).is_none() || self.node(child).is_none() {
            return;
        }
        self.detach(child);
        let (ptn, ctn) = (self.node(parent).unwrap().tn, self.node(child).unwrap().tn);
        self.node_mut(child).unwrap().parent = Some(parent);
        self.node_mut(parent).unwrap().children.push(child);
        self.taffy.add_child(ptn, ctn).expect("taffy add_child");
        self.restyle(parent); // an empty slot becomes Flex
        self.mark_dirty(parent);
    }
    fn detach(&mut self, child: NodeId) {
        let Some(parent) = self.node(child).and_then(|n| n.parent) else {
            return;
        };
        let (ptn, ctn) = (
            self.node(parent).map(|n| n.tn),
            self.node(child).unwrap().tn,
        );
        let was = self.abs_rect(child); // uncover where it was painted
        self.add_damage(was);
        if let Some(p) = self.node_mut(parent) {
            p.children.retain(|c| *c != child);
        }
        if let Some(n) = self.node_mut(child) {
            n.parent = None;
        }
        if let Some(ptn) = ptn {
            let _ = self.taffy.remove_child(ptn, ctn);
        }
        self.restyle(parent); // an emptied slot becomes Display::None
        self.mark_dirty(parent);
    }
    /// Replace the child list in `order` (used by keyed lists). Children that left
    /// the list must have been removed with `remove_subtree` beforehand.
    fn set_children(&mut self, parent: NodeId, order: Vec<NodeId>) {
        let Some(p) = self.node(parent) else { return };
        if p.children == order {
            return;
        }
        let tns: Vec<TNode> = order
            .iter()
            .filter_map(|c| self.node(*c).map(|n| n.tn))
            .collect();
        let ptn = p.tn;
        for &c in &order {
            if let Some(n) = self.node_mut(c) {
                n.parent = Some(parent);
            }
        }
        self.node_mut(parent).unwrap().children = order;
        self.taffy
            .set_children(ptn, &tns)
            .expect("taffy set_children");
        self.restyle(parent);
        self.mark_dirty(parent);
    }
    /// Idempotent and tolerant of stale ids: effects' cleanups and owner disposal may
    /// both try to remove the same subtree.
    fn remove_subtree(&mut self, id: NodeId) {
        if self.node(id).is_none() {
            return;
        }
        // One damage rect for the whole subtree (children are inside their parent).
        let was = self.abs_rect(id);
        self.add_damage(was);
        self.detach(id);
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            let Some(slot) = self.slots.get_mut(n.idx as usize) else {
                continue;
            };
            if slot.generation != n.generation {
                continue;
            }
            let Some(node) = slot.node.take() else {
                continue;
            };
            slot.generation = slot.generation.wrapping_add(1);
            self.free.push(n.idx);
            let _ = self.taffy.remove(node.tn); // parents are cleared before children are visited
            stack.extend(node.children.iter().copied());
        }
        if self.root == Some(id) {
            self.root = None;
        }
        self.dirty_layout = true;
    }

    // ── setters: compare first, then classify the change ──────────────────
    fn add_damage(&mut self, r: Rect) {
        if !r.is_empty() {
            self.damage = Some(self.damage.map_or(r, |d| d.union(r)));
        }
    }
    fn set_layout(&mut self, id: NodeId, f: impl FnOnce(&mut LayoutStyle)) {
        let changed = match self.node_mut(id) {
            Some(n) => {
                let before = n.layout.clone();
                f(&mut n.layout);
                n.layout != before
            }
            None => false,
        };
        if changed {
            self.restyle(id);
            self.mark_dirty(id);
        }
    }
    fn set_paint(&mut self, id: NodeId, f: impl FnOnce(&mut PaintStyle)) {
        let changed = match self.node_mut(id) {
            Some(n) => {
                let before = n.paint.clone();
                f(&mut n.paint);
                n.paint != before
            }
            None => false,
        };
        if changed {
            let r = self.abs_rect(id);
            self.add_damage(r); // paint-only: no layout
        }
    }
    fn set_text(&mut self, id: NodeId, s: String) {
        let Some(n) = self.node_mut(id) else { return };
        if n.text == s {
            return;
        }
        let resized = n.text.chars().count() != s.chars().count();
        n.text = s;
        let (tn, cells) = (n.tn, n.text.chars().count());
        if resized {
            // New measure context: Taffy dirties this node and its ancestors only.
            self.taffy
                .set_node_context(tn, Some(cells))
                .expect("taffy context");
            self.mark_dirty(id); // size changed → relayout this path
        } else {
            let r = self.abs_rect(id);
            self.add_damage(r); // same size → repaint only the text rect
        }
    }
    fn set_click(&mut self, id: NodeId, f: Rc<dyn Fn()>) {
        if let Some(n) = self.node_mut(id) {
            n.on_click = Some(f);
        }
    }

    // ── layout: Taffy computes (cached); we sync only what changed ─────────────
    /// Mark `id` and its ancestors dirty, stopping at the first already-dirty one
    /// (the invariant guarantees everything above it is dirty as well).
    fn mark_dirty(&mut self, mut id: NodeId) {
        self.dirty_layout = true;
        while let Some(n) = self.node_mut(id) {
            if n.dirty {
                return;
            }
            n.dirty = true;
            match n.parent {
                Some(p) => id = p,
                None => return,
            }
        }
    }

    /// Absolute rect of one node: walk UP summing parent-relative origins (O(depth)).
    fn abs_rect(&self, id: NodeId) -> Rect {
        let Some(n) = self.node(id) else {
            return Rect::default();
        };
        let (mut r, mut parent) = (n.rel, n.parent);
        while let Some(pn) = parent.and_then(|p| self.node(p)) {
            r = r.at(pn.rel.x, pn.rel.y);
            parent = pn.parent;
        }
        r
    }

    /// Taffy layout → cell rect relative to the parent. Edges are rounded individually so
    /// siblings that share an edge keep sharing it; no tree-wide pass is needed.
    fn read_rel(&self, tn: TNode) -> Rect {
        let l = self.taffy.layout(tn).expect("taffy layout");
        let (x0, y0) = (l.location.x.round(), l.location.y.round());
        Rect {
            x: x0 as i32,
            y: y0 as i32,
            w: ((l.location.x + l.size.width).round() - x0) as i32,
            h: ((l.location.y + l.size.height).round() - y0) as i32,
        }
    }

    /// Returns `(nodes synced, measure calls)` for the demo output.
    fn layout(&mut self, w: i32, h: i32) -> (usize, usize) {
        let Some(root) = self.root else {
            self.dirty_layout = false;
            return (0, 0);
        };
        if self.viewport != (w, h) {
            self.viewport = (w, h);
            self.restyle(root);
            self.mark_dirty(root);
        }
        self.dirty_layout = false;
        let tn = self.node(root).unwrap().tn;
        let avail = taffy::Size {
            width: AvailableSpace::Definite(w as f32),
            height: AvailableSpace::Definite(h as f32),
        };
        // Leaves with a context are text (n cells x 1 row); other leaves measure zero.
        // Explicit sizes arrive through `known` and win over the measured size.
        let mut measured = 0;
        self.taffy
            .compute_layout_with_measure(tn, avail, |inputs, _node, ctx, style| {
                compute_leaf_layout(
                    inputs,
                    style,
                    |_, _| 0.0,
                    |known, _avail| {
                        measured += 1;
                        let (cw, ch) = ctx.map_or((0.0, 0.0), |cells| (*cells as f32, 1.0));
                        taffy::Size {
                            width: known.width.unwrap_or(cw),
                            height: known.height.unwrap_or(ch),
                        }
                    },
                )
            })
            .expect("taffy layout");
        let mut synced = 0;
        if self.node(root).is_some_and(|n| n.dirty) {
            self.sync_dirty(root, (0, 0), (0, 0), &mut synced);
        }
        (synced, measured)
    }

    /// Visit a dirty node. `old_o`/`new_o` are the absolute origins of its parent before
    /// and after this layout. A dirty node is re-read and recursed into; a CLEAN child is
    /// only re-read for its own placement (a dirty sibling may have pushed it) and never
    /// descended: its subtree is unchanged relative to it, and if it moved, its own
    /// old+new rects already damage everything inside.
    fn sync_dirty(&mut self, id: NodeId, old_o: (i32, i32), new_o: (i32, i32), synced: &mut usize) {
        let Some(n) = self.node(id) else { return };
        let (tn, old_rel, kids) = (n.tn, n.rel, n.children.clone());
        let new_rel = self.read_rel(tn);
        let n = self.node_mut(id).unwrap();
        n.rel = new_rel;
        n.dirty = false;
        *synced += 1;

        let (old_abs, new_abs) = (old_rel.at(old_o.0, old_o.1), new_rel.at(new_o.0, new_o.1));
        if old_abs != new_abs {
            self.add_damage(old_abs); // precise: where it was + where it is now
            self.add_damage(new_abs);
        }
        for c in kids {
            let Some(cn) = self.node(c) else { continue };
            if cn.dirty {
                self.sync_dirty(c, (old_abs.x, old_abs.y), (new_abs.x, new_abs.y), synced);
            } else {
                let (ctn, c_old) = (cn.tn, cn.rel);
                let c_new = self.read_rel(ctn);
                if c_new != c_old {
                    self.node_mut(c).unwrap().rel = c_new;
                    self.add_damage(c_old.at(old_abs.x, old_abs.y));
                    self.add_damage(c_new.at(new_abs.x, new_abs.y));
                }
            }
        }
    }

    // ── input: hit-test + bubbling ─────────────────────────────────────────
    /// `(ox, oy)` = absolute origin of `id`'s parent, accumulated while descending.
    fn deepest(&self, id: NodeId, ox: i32, oy: i32, x: i32, y: i32) -> Option<NodeId> {
        let n = self.node(id)?;
        let r = n.rel.at(ox, oy);
        if !r.contains(x, y) {
            return None;
        }
        for &c in n.children.iter().rev() {
            if let Some(hit) = self.deepest(c, r.x, r.y, x, y) {
                return Some(hit);
            }
        }
        Some(id)
    }
    /// Returns the handler (cloned `Rc`) so the caller can release the tree borrow
    /// before running user code.
    fn click_handler(&self, x: i32, y: i32) -> Option<Rc<dyn Fn()>> {
        let mut cur = self.deepest(self.root?, 0, 0, x, y);
        while let Some(id) = cur {
            let n = self.node(id)?;
            if let Some(h) = &n.on_click {
                return Some(h.clone());
            }
            cur = n.parent;
        }
        None
    }

    // ── paint: display list for the damaged region ────────────────────────
    fn needs_frame(&self) -> bool {
        self.dirty_layout || self.damage.is_some()
    }
    fn take_damage(&mut self) -> Option<Rect> {
        self.damage.take()
    }
    fn paint(&self, damage: Rect) -> Vec<Cmd> {
        let mut out = vec![];
        if let Some(root) = self.root {
            self.paint_node(root, 0, 0, damage, &mut out);
        }
        out
    }
    fn paint_node(&self, id: NodeId, ox: i32, oy: i32, d: Rect, out: &mut Vec<Cmd>) {
        let Some(n) = self.node(id) else { return };
        let r = n.rel.at(ox, oy);
        if !r.intersects(&d) {
            return; // prune whole subtrees outside the damage
        }
        if let Some(ch) = n.paint.fill {
            out.push(Cmd::Fill(r, ch));
        }
        if n.layout.border {
            out.push(Cmd::Border(r));
        }
        if n.kind == Kind::Text {
            out.push(Cmd::Text {
                x: r.x,
                y: r.y,
                s: n.text.clone(),
            });
        }
        for &c in &n.children {
            self.paint_node(c, r.x, r.y, d, out);
        }
    }

    /// Verification only (walks everything on purpose): the incrementally synced rects must
    /// equal a from-scratch read of Taffy's layout for every node.
    fn verify_rects(&self) {
        fn go(t: &Tree, id: NodeId, ox: i32, oy: i32) {
            let n = t.node(id).expect("live node");
            let fresh = t.read_rel(n.tn).at(ox, oy);
            assert_eq!(t.abs_rect(id), fresh, "stale rect for a {:?} node", n.text);
            for &c in &n.children {
                go(t, c, fresh.x, fresh.y);
            }
        }
        if let Some(root) = self.root {
            go(self, root, 0, 0);
        }
    }

    /// Demo helper: find a text node by substring (to script clicks).
    fn find_text(&self, needle: &str) -> Option<Rect> {
        self.slots.iter().enumerate().find_map(|(idx, s)| {
            let n = s.node.as_ref()?;
            let r = self.abs_rect(NodeId {
                idx: idx as u32,
                generation: s.generation,
            });
            (n.kind == Kind::Text && !r.is_empty() && n.text.contains(needle)).then_some(r)
        })
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 3. Renderer boundary: a backend-agnostic display list
//    Swap `AsciiRenderer` for tiny-skia now and a Vulkan backend later; nothing above
//    this line knows what a pixel is.
// ════════════════════════════════════════════════════════════════════════════

enum Cmd {
    Fill(Rect, char),
    Border(Rect),
    Text { x: i32, y: i32, s: String },
}

trait Renderer {
    /// Repaint `damage` using `list`. Returns the number of cells/pixels touched.
    fn render(&mut self, list: &[Cmd], damage: Rect) -> i32;
    fn present(&self) -> String;
}

struct AsciiRenderer {
    w: i32,
    h: i32,
    buf: Vec<char>, // persistent framebuffer: untouched cells keep their previous content
}

impl AsciiRenderer {
    fn new(w: i32, h: i32) -> Self {
        Self {
            w,
            h,
            buf: vec![' '; (w * h) as usize],
        }
    }
    fn put(&mut self, clip: &Rect, x: i32, y: i32, ch: char) {
        if clip.contains(x, y) {
            self.buf[(y * self.w + x) as usize] = ch;
        }
    }
}

impl Renderer for AsciiRenderer {
    fn render(&mut self, list: &[Cmd], damage: Rect) -> i32 {
        let d = damage.intersect(Rect {
            x: 0,
            y: 0,
            w: self.w,
            h: self.h,
        });
        for y in d.y..d.y + d.h {
            for x in d.x..d.x + d.w {
                self.put(&d, x, y, ' '); // clear only the damaged region
            }
        }
        for cmd in list {
            match cmd {
                Cmd::Fill(r, ch) => {
                    for y in r.y..r.y + r.h {
                        for x in r.x..r.x + r.w {
                            self.put(&d, x, y, *ch);
                        }
                    }
                }
                Cmd::Border(r) if r.w >= 2 && r.h >= 2 => {
                    let (x1, y1) = (r.x + r.w - 1, r.y + r.h - 1);
                    for x in r.x + 1..x1 {
                        self.put(&d, x, r.y, '─');
                        self.put(&d, x, y1, '─');
                    }
                    for y in r.y + 1..y1 {
                        self.put(&d, r.x, y, '│');
                        self.put(&d, x1, y, '│');
                    }
                    self.put(&d, r.x, r.y, '┌');
                    self.put(&d, x1, r.y, '┐');
                    self.put(&d, r.x, y1, '└');
                    self.put(&d, x1, y1, '┘');
                }
                Cmd::Border(_) => {}
                Cmd::Text { x, y, s } => {
                    for (i, ch) in s.chars().enumerate() {
                        self.put(&d, x + i as i32, *y, ch);
                    }
                }
            }
        }
        d.w * d.h
    }
    fn present(&self) -> String {
        self.buf
            .chunks(self.w as usize)
            .map(|row| row.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 4. View layer: builders + control flow. Components run once; dynamic parts are
//    effects that write into the Tree.
// ════════════════════════════════════════════════════════════════════════════

/// Build context. Cheap to create; do NOT capture it in stored closures (rule 6).
#[derive(Clone)]
struct Cx {
    rt: Runtime,
    tree: Rc<RefCell<Tree>>,
}

/// Chainable handle to a node being built.
struct V<'a> {
    cx: &'a Cx,
    id: NodeId,
}

impl<'a> From<V<'a>> for NodeId {
    fn from(v: V<'a>) -> NodeId {
        v.id
    }
}

impl<'a> V<'a> {
    fn id(self) -> NodeId {
        self.id
    }
    fn layout(self, f: impl FnOnce(&mut LayoutStyle)) -> Self {
        self.cx.tree.borrow_mut().set_layout(self.id, f);
        self
    }
    fn paint(self, f: impl FnOnce(&mut PaintStyle)) -> Self {
        self.cx.tree.borrow_mut().set_paint(self.id, f);
        self
    }
    fn pad(self, n: i32) -> Self {
        self.layout(|l| l.pad = n)
    }
    fn gap(self, n: i32) -> Self {
        self.layout(|l| l.gap = n)
    }
    fn border(self) -> Self {
        self.layout(|l| l.border = true)
    }
    fn width(self, n: i32) -> Self {
        self.layout(|l| l.width = Some(n))
    }
    fn height(self, n: i32) -> Self {
        self.layout(|l| l.height = Some(n))
    }
    fn grow(self) -> Self {
        self.layout(|l| l.grow = true)
    }
    fn fill(self, ch: char) -> Self {
        self.paint(|p| p.fill = Some(ch))
    }
    fn child(self, c: impl Into<NodeId>) -> Self {
        self.cx.tree.borrow_mut().append(self.id, c.into());
        self
    }
    fn on_click(self, f: impl Fn() + 'static) -> Self {
        self.cx.tree.borrow_mut().set_click(self.id, Rc::new(f));
        self
    }
    /// Reactive binding: paint-only (never triggers layout).
    fn bind_fill(self, f: impl Fn() -> Option<char> + 'static) -> Self {
        bind(self.cx, self.id, f, |t, id, c| {
            t.set_paint(id, |p| p.fill = c)
        });
        self
    }
    /// Reactive binding: geometry (triggers layout when the value changes).
    fn bind_width(self, f: impl Fn() -> i32 + 'static) -> Self {
        bind(self.cx, self.id, f, |t, id, w| {
            t.set_layout(id, |l| l.width = Some(w))
        });
        self
    }
}

/// The ONE primitive that connects signals to the tree: an effect that evaluates `f`
/// (tracking whatever signals it reads) and applies the result to the node.
/// Exactly one effect per dynamic property → updates are as granular as the reads.
fn bind<T: 'static>(
    cx: &Cx,
    id: NodeId,
    f: impl Fn() -> T + 'static,
    apply: impl Fn(&mut Tree, NodeId, T) + 'static,
) {
    let tree = cx.tree.clone();
    cx.rt
        .effect(move |_| {
            let v = f(); // tracked reads happen here
            apply(&mut tree.borrow_mut(), id, v); // setters only compare + mark dirty
            Ok(())
        })
        .expect("effect");
}

fn div(cx: &Cx) -> V<'_> {
    V {
        cx,
        id: cx.tree.borrow_mut().create(Kind::Box),
    }
}
fn row(cx: &Cx) -> V<'_> {
    div(cx).layout(|l| l.dir = Dir::Row)
}
fn text<'a>(cx: &'a Cx, s: &str) -> V<'a> {
    let id = cx.tree.borrow_mut().create(Kind::Text);
    cx.tree.borrow_mut().set_text(id, s.to_string());
    V { cx, id }
}
fn text_dyn<'a>(cx: &'a Cx, f: impl Fn() -> String + 'static) -> V<'a> {
    let id = cx.tree.borrow_mut().create(Kind::Text);
    bind(cx, id, f, |t, id, s| t.set_text(id, s));
    V { cx, id }
}
fn button<'a>(cx: &'a Cx, label: &str, f: impl Fn() + 'static) -> V<'a> {
    div(cx).child(text(cx, &format!("[ {label} ]"))).on_click(f)
}

/// Conditional subtree.
/// * the memo cuts re-runs: the effect only wakes when the *boolean* flips;
/// * the effect re-run disposes everything the previous run created (signals, memos,
///   nested effects, cleanups) → we only register the node-removal cleanup.
fn show<'a>(
    cx: &'a Cx,
    when: impl Fn() -> bool + 'static,
    then: impl Fn(&Cx) -> NodeId + 'static,
) -> V<'a> {
    let slot = cx.tree.borrow_mut().create(Kind::Slot);
    let cond = cx
        .rt
        .memo(move |_| Ok::<bool, RuntimeError>(when()))
        .expect("memo");
    let tree = cx.tree.clone();
    cx.rt
        .effect(move |rt| {
            if cond.try_get_copy(rt)? {
                let cx = Cx {
                    rt: rt.clone(),
                    tree: tree.clone(),
                };
                // untrack: signals read while *building* must not re-run this effect.
                let child = rt.untrack(|_| then(&cx));
                tree.borrow_mut().append(slot, child);
                let t = tree.clone();
                rt.on_cleanup(move |_| {
                    t.borrow_mut().remove_subtree(child);
                    Ok(())
                })?;
            }
            Ok(())
        })
        .expect("effect");
    V { cx, id: slot }
}

/// Keyed list. Each row has its OWN owner (a child of the owner current at creation),
/// deliberately NOT created inside this effect's execution scope — otherwise the next
/// run of the effect would dispose every row. Keys must be unique.
fn for_each<'a, K>(
    cx: &'a Cx,
    list: impl Fn() -> Vec<K> + 'static,
    item: impl Fn(&Cx, &K) -> NodeId + 'static,
) -> V<'a>
where
    K: Clone + Eq + Hash + 'static,
{
    let slot = cx.tree.borrow_mut().create(Kind::Slot);
    let tree = cx.tree.clone();
    let scope: OwnerHandle = cx
        .rt
        .current_owner()
        .expect("current_owner")
        .expect("for_each needs a current owner");
    let rows: Rc<RefCell<HashMap<K, (OwnerHandle, NodeId)>>> = Default::default();

    cx.rt
        .effect(move |rt| {
            let keys = list(); // tracked: the effect re-runs when the list changes
            rt.untrack(|rt| -> Result<(), RuntimeError> {
                let mut rows = rows.borrow_mut();
                let alive: HashSet<&K> = keys.iter().collect();

                // 1. rows that left: dispose owner (cleanups, nested effects) + remove nodes
                let gone: Vec<K> = rows
                    .keys()
                    .filter(|k| !alive.contains(k))
                    .cloned()
                    .collect();
                for k in gone {
                    let (owner, node) = rows.remove(&k).unwrap();
                    rt.dispose(owner)?;
                    tree.borrow_mut().remove_subtree(node);
                }
                // 2. rows that arrived: build once, inside their own owner
                let mut order = Vec::with_capacity(keys.len());
                for k in &keys {
                    if !rows.contains_key(k) {
                        let owner = rt.create_owner(scope)?;
                        let node = rt.with_owner(owner, |rt| {
                            let cx = Cx {
                                rt: rt.clone(),
                                tree: tree.clone(),
                            };
                            Ok(item(&cx, k))
                        })?;
                        rows.insert(k.clone(), (owner, node));
                    }
                    order.push(rows[k].1);
                }
                // 3. (re)order children
                tree.borrow_mut().set_children(slot, order);
                Ok(())
            })
        })
        .expect("effect");
    V { cx, id: slot }
}

// ════════════════════════════════════════════════════════════════════════════
// 5. Host: the event loop (stand-in for wayland-client + calloop)
// ════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
enum Event {
    Click(i32, i32),
    Tick,
}

struct Host {
    rt: Runtime,
    root: OwnerHandle,
    tree: Rc<RefCell<Tree>>,
    wake: Rc<Cell<bool>>, // set by the flush hook; in real life: calloop Ping/eventfd
    renderer: AsciiRenderer,
    w: i32,
    h: i32,
    iteration: u32,
}

impl Host {
    fn new(w: i32, h: i32, app: impl FnOnce(&Cx) -> NodeId) -> Self {
        let rt = Runtime::new();
        RT.with(|c| *c.borrow_mut() = Some(rt.clone()));

        // The hook may NOT read/mutate reactive state; it only wakes the loop. The runtime
        // coalesces: one request stays outstanding until the next `flush`.
        let wake = Rc::new(Cell::new(false));
        let w2 = wake.clone();
        rt.set_request_flush_hook(move |_| w2.set(true))
            .expect("hook");

        let tree = Rc::new(RefCell::new(Tree::new()));
        let root = rt.create_root().expect("root");
        let cx = Cx {
            rt: rt.clone(),
            tree: tree.clone(),
        };
        rt.with_owner(root, |rt| {
            let id = app(&cx); // component bodies run exactly once
            tree.borrow_mut().set_root(id);
            let t = tree.clone();
            rt.on_cleanup(move |_| {
                t.borrow_mut().remove_subtree(id);
                Ok(())
            })
        })
        .expect("mount");

        Host {
            rt,
            root,
            tree,
            wake,
            renderer: AsciiRenderer::new(w, h),
            w,
            h,
            iteration: 0,
        }
    }

    fn dispatch(&self, ev: Event) {
        // Clone the handler out, release the tree borrow, THEN run user code.
        let handler = match ev {
            Event::Click(x, y) => self.tree.borrow().click_handler(x, y),
            Event::Tick => self.tree.borrow().on_tick.clone(),
        };
        if let Some(h) = handler {
            h();
        }
    }

    /// One loop turn: input → batch(handlers) → flush if woken → frame if dirty.
    fn iteration(&mut self, label: &str, events: Vec<Event>) {
        self.iteration += 1;
        let n = events.len();

        // 1. Input. batch() only defers the host notification; writes are immediate and
        //    no effect runs here, so handlers cannot re-enter the tree.
        if n > 0 {
            self.rt
                .batch(|_| {
                    for ev in events {
                        self.dispatch(ev);
                    }
                    Ok(())
                })
                .expect("batch");
        }

        // 2. Reactive flush — only if the hook asked for it.
        let report = if self.wake.replace(false) {
            Some(self.rt.flush(FlushOptions::default()).expect("flush"))
        } else {
            None
        };

        // 3. "Frame callback": layout (if needed) → paint damaged region → present.
        let frame = {
            let mut t = self.tree.borrow_mut();
            if !t.needs_frame() {
                None
            } else {
                let info = if t.dirty_layout {
                    let (synced, measured) = t.layout(self.w, self.h);
                    Some((synced, measured, t.live_nodes()))
                } else {
                    None
                };
                t.take_damage().map(|d| (d, info, t.paint(d)))
            }
        };

        self.tree.borrow().verify_rects();

        println!("── iteration {} · {label} · events={n} ──", self.iteration);
        match &report {
            Some(r) => println!(
                "flush: rounds={} effect_runs={} outcome={:?}",
                r.rounds, r.callbacks, r.outcome
            ),
            None => println!("flush: not requested (no observed signal changed)"),
        }
        match frame {
            Some((d, info, list)) => {
                let cells = self.renderer.render(&list, d);
                let layout = match info {
                    Some((synced, measured, total)) => {
                        format!("taffy synced={synced}/{total} nodes, measured={measured}")
                    }
                    None => "skipped".to_string(),
                };
                println!(
                    "frame: layout={layout} damage={d:?} cells_repainted={cells} cmds={}",
                    list.len()
                );
                println!("{}", self.renderer.present());
            }
            None => println!("frame: skipped (nothing dirty)"),
        }
        println!();
    }

    fn locate(&self, label: &str) -> Event {
        let r = self
            .tree
            .borrow()
            .find_text(label)
            .unwrap_or_else(|| panic!("`{label}` not on screen"));
        Event::Click(r.x, r.y)
    }

    fn shutdown(self) {
        self.rt.dispose(self.root).expect("dispose"); // runs cleanups: removes all nodes
        self.rt.shutdown().expect("shutdown");
        let left = self.tree.borrow().live_nodes();
        println!("shutdown: live UI nodes = {left}");
        assert_eq!(left, 0, "UI nodes leaked");
        RT.with(|c| drop(c.borrow_mut().take()));
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 6. Application: written once, in terms of signals and builders
// ════════════════════════════════════════════════════════════════════════════

fn app(cx: &Cx) -> NodeId {
    let count = Sig::new(0i32);
    let items = Sig::new(vec![1u32, 2, 3]);
    let next_id = Sig::new(4u32);
    let progress = Sig::new(0i32);

    // Time-driven state: a tick handler (real animation would interpolate node props
    // directly and only publish start/end through signals).
    cx.tree.borrow_mut().on_tick = Some(Rc::new(move || progress.update(|p| *p = (*p + 4) % 21)));

    let is_even = move || count.get() % 2 == 0;

    div(cx)
        .border()
        .pad(1)
        .gap(1)
        // Row 1: text (size may change → layout) + paint-only parity bar
        .child(
            row(cx)
                .gap(2)
                .child(text_dyn(cx, move || format!("count = {}", count.get())))
                .child(
                    div(cx)
                        .width(10)
                        .height(1)
                        .bind_fill(move || Some(if count.get() % 2 == 0 { '#' } else { '.' })),
                ),
        )
        // Row 2: buttons + conditional subtree
        .child(
            row(cx)
                .gap(2)
                .child(button(cx, "+1", move || count.update(|n| *n += 1)))
                .child(button(cx, "-1", move || count.update(|n| *n -= 1)))
                .child(show(cx, is_even, |cx| text(cx, "even!").id())),
        )
        // Row 3: list mutations
        .child(
            row(cx)
                .gap(2)
                .child(button(cx, "add", move || {
                    let id = next_id.get();
                    next_id.set(id + 1);
                    items.update(|v| v.push(id));
                }))
                .child(button(cx, "pop", move || {
                    items.update(|v| {
                        v.pop();
                    })
                })),
        )
        // Keyed list: rows are built once; only their inner bindings re-run on `count`
        .child(for_each(
            cx,
            move || items.get_cloned(),
            move |cx, &id| {
                row(cx)
                    .gap(1)
                    .child(text(cx, &format!("- item {id}")))
                    .child(text_dyn(cx, move || {
                        format!("(count*{id} = {})", count.get() * id as i32)
                    }))
                    .id()
            },
        ))
        // Geometry bound to a signal
        .child(
            row(cx).child(text(cx, "progress ")).child(
                div(cx)
                    .height(1)
                    .fill('#')
                    .bind_width(move || progress.get()),
            ),
        )
        .id()
}

fn main() {
    let mut host = Host::new(40, 18, app);

    // Mount: effects run in rounds (nested effects created during a round run in the next).
    host.iteration("mount", vec![]);

    // Same-size text change + paint-only parity bar + `show` flips → small damage.
    let plus = host.locate("+1");
    host.iteration("click +1", vec![plus]);

    // Two writes in ONE loop turn: one flush, one frame (coalescing).
    let plus = host.locate("+1");
    host.iteration("click +1 twice (coalesced)", vec![plus, plus]);

    // New keyed row: only the new row is built.
    let add = host.locate("add");
    host.iteration("add item", vec![add]);

    // Signal-bound geometry (progress bar width).
    host.iteration("3 ticks", vec![Event::Tick, Event::Tick, Event::Tick]);

    // Row owner disposed: its effects and nodes disappear.
    let pop = host.locate("pop");
    host.iteration("pop item", vec![pop]);

    // Seven writes, one flush; the memo behind `show` cuts redundant effect wake-ups;
    // "count = 10" is wider → layout.
    let plus = host.locate("+1");
    host.iteration("click +1 x7", vec![plus; 7]);

    // Click on nothing: no signal changes → no flush, no frame.
    host.iteration("click on empty space", vec![Event::Click(39, 17)]);

    host.shutdown();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 400 pseudo-random interactions (1-3 events per loop turn). After every frame
    /// `Host::iteration` asserts that the incrementally synced rects equal a full
    /// recompute, so a stale rect anywhere fails the test.
    #[test]
    fn incremental_sync_matches_full_recompute() {
        let mut host = Host::new(40, 18, app);
        host.iteration("mount", vec![]);
        let mut seed = 0x2545_F491_u32;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for _ in 0..400 {
            let events = (0..=next() % 3)
                .map(|_| match next() % 6 {
                    0 => host.locate("[ +1 ]"),
                    1 => host.locate("[ -1 ]"),
                    2 => host.locate("[ add ]"),
                    3 => host.locate("[ pop ]"),
                    4 => Event::Tick,
                    _ => Event::Click(39, 17),
                })
                .collect();
            host.iteration("fuzz", events);
        }
        host.shutdown();
    }
}
