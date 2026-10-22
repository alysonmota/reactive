//! ui_window.rs — the reactive UI architecture, now on real pixels:
//! wgpu GPU rasterization, cosmic-text shaping, Taffy layout, and a direct Wayland GPU surface
//! driven by calloop (wl_shm is not used for window presentation).
//!
//!     cargo run --profile fast-dev --bin ui-window                  # opens the window (30 Hz animation)
//!     cargo run --profile fast-dev --bin ui-window -- --static      # same, no animation timer
//!     cargo run --profile fast-dev --bin ui-window -- --frames 30   # exit after 30 commits (smoke test)
//!     cargo run --profile fast-dev --bin ui-window -- --png out.png # headless: scripted run → PNG
//!     UI_LOG=1 …                                                    # one stats line per presented frame
//!     cargo test --profile fast-dev                                 # layout/damage/buffer fuzz tests
//!     cargo run --profile fast-dev --features paint-flash …         # flash every repainted container red
//!
//! Cargo.toml (besides `reactive`):
//!     [features]
//!     paint-flash = []   # debug overlay, see rule 11; nothing of it exists when off
//!     taffy = { version = "0.15", default-features = false, features = ["std", "flexbox", "taffy_tree"] }
//!     wgpu = "0.19"
//!     pollster = "0.3"
//!     bytemuck = { version = "1", features = ["derive"] }
//!     png = "0.17"
//!     cosmic-text = "0.19"
//!     wayland-client = { version = "0.31", features = ["system"] }
//!     wayland-protocols = { version = "0.32", features = ["client"] }
//!     calloop = "0.14"
//!     calloop-wayland-source = "0.4"
//!     raw-window-handle = "0.6"
//!
//! Not covered (on purpose): HiDPI scale (buffers are 1:1 logical pixels), keyboard input,
//! cursor shape, clipboard, and window decorations. Close with the compositor's own binding.
//!
//! ───────────────────────────── data flow ─────────────────────────────
//!
//!   wl_pointer ─► Event ─► handlers (inside Runtime::batch) ─► Sig::set     no effect runs
//!                                                  │ marks observers, enqueues effects
//!                                                  ▼
//!                          flush hook ─► calloop Ping ─► Runtime::flush
//!                                                  │ ONLY bindings whose sources changed run;
//!                                                  ▼ they write to the Tree (compare, then mark)
//!                      Tree: dirty path (layout) / damage rect (paint)
//!                                                  │
//!                                                  ▼ present(): once per wl_surface.frame
//!       Taffy (cached) ─► sync dirty paths ─► display list ─► WgpuRenderer ─► damage only
//!                                                  │
//!                                                  ▼
//!              persistent GPU target ─► fullscreen blit ─► wgpu surface present
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
//!  7. Every UI node owns a Taffy node; `Tree` setters mirror structure/style/text into it.
//!     Empty `Slot`s are `Display::None`, so an empty `show` adds neither size nor gap.
//!  8. Layout is incremental end to end: setters mark the node and its ancestors dirty, Taffy
//!     recomputes the dirty path (it caches per node), and `sync_dirty` visits only that path.
//!     Nodes store PARENT-RELATIVE rects; absolute positions are derived top-down in
//!     paint/hit-test, or by walking up (`abs_rect`) when one node needs damage.
//!     Taffy's rounding pass walks the whole tree, so it is off; edges snap per node.
//!  9. Text is shaped once per change (`Tree::shape`): the size feeds Taffy through the node
//!     context, the positioned glyph keys ride in the display list, and only the renderer
//!     rasterizes them. Units are logical pixels at scale 1.
//! 10. The offscreen GPU texture is persistent, so only damaged rectangles are rerasterized.
//!     A fullscreen GPU blit copies it to each acquired swapchain image.
//! 11. `paint-flash` (cfg feature, zero cost when off: the field, the hooks and the display-list
//!     command are not compiled). Every rect that `Tree::add_damage` receives is an "updated
//!     container"; each frame's set is tinted translucent red for `FLASH_LIFETIME`, then repainted
//!     clean. Highlights still alive are re-tinted wherever a later repaint wipes them. Elements
//!     that were not updated are never touched. Use it to see what an interaction really repaints.

#![allow(dead_code)]

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet},
    hash::Hash,
    rc::Rc,
};

use cosmic_text::{
    Attrs, Buffer, CacheKey, Family, FontSystem, Metrics, Shaping, SwashCache, SwashContent,
};
use reactive::{FlushOptions, FlushReport, OwnerHandle, Runtime, RuntimeError, Signal};
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
// 2. ui-core: geometry, styles, generational node tree, dirty tracking, text shaping
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

/// Straight-alpha RGBA.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Color(u8, u8, u8, u8);

mod theme {
    use super::Color;
    pub const BG: Color = Color(0x12, 0x14, 0x1a, 255);
    pub const SURFACE: Color = Color(0x1e, 0x22, 0x2c, 255);
    pub const BORDER: Color = Color(0x34, 0x3a, 0x4a, 255);
    pub const TRACK: Color = Color(0x2c, 0x32, 0x40, 255);
    pub const TEXT: Color = Color(0xe8, 0xeb, 0xf2, 255);
    pub const MUTED: Color = Color(0x8c, 0x94, 0xa6, 255);
    pub const ACCENT: Color = Color(0x5c, 0x7c, 0xfa, 255);
    pub const ACCENT_HI: Color = Color(0x7c, 0x96, 0xff, 255);
    pub const GOOD: Color = Color(0x48, 0xc8, 0x8c, 255);
}

#[derive(Clone, Copy, PartialEq, Default, Debug)]
enum Dir {
    #[default]
    Col,
    Row,
}

/// Properties that can change geometry → setting one dirties LAYOUT. Logical pixels.
#[derive(Clone, PartialEq, Default)]
struct LayoutStyle {
    dir: Dir,
    gap: i32,
    pad_x: i32,
    pad_y: i32,
    border: i32, // width; occupies space like padding
    width: Option<i32>,
    height: Option<i32>,
    grow: bool,   // takes a share of leftover main-axis space
    center: bool, // centers children on the cross axis
}

/// Properties that only change pixels → setting one only adds a damage rect.
#[derive(Clone, PartialEq)]
struct PaintStyle {
    fill: Option<Color>,
    radius: i32,
    border_color: Color,
    color: Color, // text color
}

impl Default for PaintStyle {
    fn default() -> Self {
        PaintStyle {
            fill: None,
            radius: 0,
            border_color: theme::BORDER,
            color: theme::TEXT,
        }
    }
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

/// A shaped glyph, positioned relative to the text origin. Rasterized only by the renderer.
#[derive(Clone, Copy)]
struct Glyph {
    key: CacheKey,
    x: i32,
    y: i32,
}

const FONT_PX: f32 = 15.0;

struct Node {
    kind: Kind,
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    layout: LayoutStyle,
    paint: PaintStyle,
    text: String,
    font_px: f32,
    glyphs: Rc<[Glyph]>,
    measured: (f32, f32), // shaped text size, mirrored into Taffy's node context
    rel: Rect,            // parent-relative placement, as of the last sync (= last painted)
    dirty: bool,          // layout may have changed here or below; invariant: parent is dirty too
    tn: TNode,            // the mirrored Taffy node
    on_click: Option<Rc<dyn Fn()>>,
    on_hover: Option<Rc<dyn Fn(bool)>>,
}

struct Slot {
    generation: u32,
    node: Option<Node>,
}

/// A hover handler to run, with `true` = pointer entered, `false` = left.
type HoverCall = (Rc<dyn Fn(bool)>, bool);

/// Shared by the tree (shaping) and the renderer (glyph images). Single-threaded.
type Fonts = Rc<RefCell<FontSystem>>;

struct Tree {
    slots: Vec<Slot>,
    free: Vec<u32>,
    root: Option<NodeId>,
    dirty_layout: bool,
    damage: Option<Rect>, // union of regions that need repaint
    on_tick: Option<Rc<dyn Fn()>>,
    hover: Option<NodeId>,
    taffy: TaffyTree<(f32, f32)>, // node context = shaped text size in px
    viewport: (i32, i32),         // the root fills it
    fonts: Fonts,
    scratch: Buffer, // reused for every shaping call
    #[cfg(feature = "paint-flash")]
    flash: Vec<Rect>, // containers updated since the last frame
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
        // Children keep their own cross size unless centered; Taffy's default is stretch.
        align_items: if ls.center {
            AlignItems::CENTER
        } else {
            AlignItems::FLEX_START
        },
        gap: taffy::Size {
            width: len(ls.gap),
            height: len(ls.gap),
        },
        padding: taffy::Rect {
            left: len(ls.pad_x),
            right: len(ls.pad_x),
            top: len(ls.pad_y),
            bottom: len(ls.pad_y),
        },
        border: {
            let b = len(ls.border);
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
        flex_shrink: 0.0, // pixels don't shrink
        ..Style::default()
    }
}

/// Shape `text` on one unwrapped line. Returns the positioned glyphs and the pixel size.
/// This is the only place that talks to cosmic-text for layout purposes.
fn shape(fonts: &Fonts, scratch: &mut Buffer, text: &str, px: f32) -> (Rc<[Glyph]>, (f32, f32)) {
    let mut fs = fonts.borrow_mut();
    scratch.set_metrics(Metrics::new(px, (px * 1.35).round()));
    scratch.set_text(
        text,
        &Attrs::new().family(Family::SansSerif),
        Shaping::Advanced,
        None,
    );
    scratch.shape_until_scroll(&mut fs, false);
    let (mut w, mut h) = (0f32, 0f32);
    let mut glyphs = Vec::new();
    for run in scratch.layout_runs() {
        w = w.max(run.line_w);
        h = h.max(run.line_top + run.line_height);
        for g in run.glyphs {
            let p = g.physical((0.0, run.line_y), 1.0);
            glyphs.push(Glyph {
                key: p.cache_key,
                x: p.x,
                y: p.y,
            });
        }
    }
    (glyphs.into(), (w.ceil(), h.ceil()))
}

impl Tree {
    fn new(fonts: Fonts) -> Self {
        let mut taffy = TaffyTree::new();
        // Taffy's rounding pass visits the whole tree on every compute, so it is off;
        // `read_rel` snaps each node's edges to pixels itself (O(visited)).
        taffy.disable_rounding();
        let scratch = Buffer::new(&mut fonts.borrow_mut(), Metrics::new(FONT_PX, 20.0));
        Tree {
            slots: vec![],
            free: vec![],
            root: None,
            dirty_layout: false,
            damage: None,
            on_tick: None,
            hover: None,
            taffy,
            viewport: (0, 0),
            fonts,
            scratch,
            #[cfg(feature = "paint-flash")]
            flash: vec![],
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
            Kind::Text => self.taffy.new_leaf_with_context(style, (0.0, 0.0)),
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
            font_px: FONT_PX,
            glyphs: Rc::from([]),
            measured: (0.0, 0.0),
            rel: Rect::default(),
            dirty: true, // never placed yet
            tn,
            on_click: None,
            on_hover: None,
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
    /// Window resized: the root restyles to the new viewport and everything repaints.
    fn resize(&mut self, w: i32, h: i32) {
        self.viewport = (w, h);
        if let Some(root) = self.root {
            self.restyle(root);
            self.mark_dirty(root);
        }
        self.add_damage(Rect { x: 0, y: 0, w, h });
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
    /// Something visible changed inside `r` (an updated container).
    fn add_damage(&mut self, r: Rect) {
        if !r.is_empty() {
            #[cfg(feature = "paint-flash")]
            self.flash.push(r);
            self.push_damage(r);
        }
    }
    /// Repaint `r` without counting it as an update (used to wipe expired highlights).
    fn push_damage(&mut self, r: Rect) {
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
        n.text = s;
        self.reshape(id);
    }
    fn set_font(&mut self, id: NodeId, px: f32) {
        let Some(n) = self.node_mut(id) else { return };
        if n.font_px == px {
            return;
        }
        n.font_px = px;
        self.reshape(id);
    }
    /// Re-shape a text node. A different size is a layout change (Taffy gets a new measure
    /// context and dirties only this node and its ancestors); the same size only repaints.
    fn reshape(&mut self, id: NodeId) {
        let Some(n) = self.node(id) else { return };
        let (text, px, tn, old) = (n.text.clone(), n.font_px, n.tn, n.measured);
        let (glyphs, size) = shape(&self.fonts, &mut self.scratch, &text, px);
        let n = self.node_mut(id).unwrap();
        n.glyphs = glyphs;
        n.measured = size;
        if size != old {
            self.taffy
                .set_node_context(tn, Some(size))
                .expect("taffy context");
            self.mark_dirty(id);
        } else {
            let r = self.abs_rect(id);
            self.add_damage(r);
        }
    }
    fn set_click(&mut self, id: NodeId, f: Rc<dyn Fn()>) {
        if let Some(n) = self.node_mut(id) {
            n.on_click = Some(f);
        }
    }
    fn set_hover(&mut self, id: NodeId, f: Rc<dyn Fn(bool)>) {
        if let Some(n) = self.node_mut(id) {
            n.on_hover = Some(f);
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

    /// Taffy layout → pixel rect relative to the parent. Edges are rounded individually so
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

    /// Returns `(nodes synced, measure calls)` for the stats line.
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
        // Leaves with a context are text (their shaped size); other leaves measure zero.
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
                        let (cw, ch) = ctx.map_or((0.0, 0.0), |s| *s);
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
    /// Nearest ancestor-or-self of the node under (x, y) that satisfies `pred`.
    fn bubble(&self, x: i32, y: i32, pred: impl Fn(&Node) -> bool) -> Option<NodeId> {
        let mut cur = self.deepest(self.root?, 0, 0, x, y);
        while let Some(id) = cur {
            let n = self.node(id)?;
            if pred(n) {
                return Some(id);
            }
            cur = n.parent;
        }
        None
    }
    /// Returns the handler (cloned `Rc`) so the caller can release the tree borrow
    /// before running user code.
    fn click_handler(&self, x: i32, y: i32) -> Option<Rc<dyn Fn()>> {
        let id = self.bubble(x, y, |n| n.on_click.is_some())?;
        self.node(id)?.on_click.clone()
    }
    /// Pointer moved: returns the enter/leave handlers to run (leave first), already
    /// cloned out of the tree. `None` coordinates mean the pointer left the window.
    fn hover_move(&mut self, at: Option<(i32, i32)>) -> Vec<HoverCall> {
        let target = at.and_then(|(x, y)| self.bubble(x, y, |n| n.on_hover.is_some()));
        if target == self.hover {
            return vec![];
        }
        let mut out = vec![];
        if let Some(h) = self
            .hover
            .and_then(|o| self.node(o))
            .and_then(|n| n.on_hover.clone())
        {
            out.push((h, false));
        }
        if let Some(h) = target
            .and_then(|t| self.node(t))
            .and_then(|n| n.on_hover.clone())
        {
            out.push((h, true));
        }
        self.hover = target;
        out
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
        if let Some(fill) = n.paint.fill {
            out.push(Cmd::Fill {
                r,
                radius: n.paint.radius,
                color: fill,
            });
        }
        if n.layout.border > 0 {
            out.push(Cmd::Border {
                r,
                radius: n.paint.radius,
                width: n.layout.border,
                color: n.paint.border_color,
            });
        }
        if n.kind == Kind::Text && !n.glyphs.is_empty() {
            out.push(Cmd::Text {
                x: r.x,
                y: r.y,
                color: n.paint.color,
                glyphs: n.glyphs.clone(),
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

    /// Demo helper: absolute rect of the text node whose content is exactly `text`.
    fn find_text(&self, text: &str) -> Option<Rect> {
        self.slots.iter().enumerate().find_map(|(idx, s)| {
            let n = s.node.as_ref()?;
            let r = self.abs_rect(NodeId {
                idx: idx as u32,
                generation: s.generation,
            });
            (n.kind == Kind::Text && !r.is_empty() && n.text == text).then_some(r)
        })
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 3. Renderer boundary: wgpu rasterization (GPU) + optional headless readback.
//    The Wayland path samples the persistent GPU target directly into the swapchain;
//    CPU readback is enabled only for headless PNG output and tests.
// ════════════════════════════════════════════════════════════════════════════

enum Cmd {
    Fill {
        r: Rect,
        radius: i32,
        color: Color,
    },
    Border {
        r: Rect,
        radius: i32,
        width: i32,
        color: Color,
    },
    Text {
        x: i32,
        y: i32,
        color: Color,
        glyphs: Rc<[Glyph]>,
    },
    /// Translucent overlay over the UNION of `rects` (overlaps are not tinted twice).
    #[cfg(feature = "paint-flash")]
    Tint {
        rects: Vec<Rect>,
        color: Color,
    },
}

trait Renderer {
    fn resize(&mut self, w: i32, h: i32);
    /// Repaint `damage` (clamped to the surface) from `list`; returns what was repainted.
    fn render(&mut self, list: &[Cmd], damage: Rect) -> Rect;
    /// CPU copy used only for headless PNG output and regression tests; Wayland presents from GPU memory.
    fn framebuffer(&self) -> &[u8];
}

const GLYPH_ATLAS_SIZE: u32 = 2048;

const WGSL: &str = r#"
struct VertexIn {
    @location(0) position: vec2<f32>,
    @location(1) local: vec2<f32>,
    @location(2) size: vec2<f32>,
    @location(3) misc: vec4<f32>, // radius, border width, shape mode, glyph mode
    @location(4) color: vec4<f32>,
    @location(5) uv: vec2<f32>,
};
struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) misc: vec4<f32>,
    @location(3) color: vec4<f32>,
    @location(4) uv: vec2<f32>,
};
@group(0) @binding(0) var glyph_atlas: texture_2d<f32>;
@group(0) @binding(1) var glyph_sampler: sampler;

@vertex
fn vs_main(v: VertexIn) -> VertexOut {
    var o: VertexOut;
    o.position = vec4<f32>(v.position, 0.0, 1.0);
    o.local = v.local;
    o.size = v.size;
    o.misc = v.misc;
    o.color = v.color;
    o.uv = v.uv;
    return o;
}

fn rounded_rect_distance(point: vec2<f32>, size: vec2<f32>, radius: f32) -> f32 {
    let half_size = size * 0.5;
    let r = clamp(radius, 0.0, min(half_size.x, half_size.y));
    let q = abs(point - half_size) - (half_size - vec2<f32>(r, r));
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_shape(i: VertexOut) -> @location(0) vec4<f32> {
    if i.misc.z > 1.5 {
        // Opaque damage clear; unlike an ordinary shape it must cover its exact edges.
        return i.color;
    }
    let d_outer = rounded_rect_distance(i.local, i.size, i.misc.x);
    let aa = max(fwidth(d_outer), 0.65);
    let outer = 1.0 - smoothstep(-aa, aa, d_outer);
    var coverage = outer;
    if i.misc.z > 0.5 {
        let inset = i.misc.y;
        let inner_size = max(i.size - vec2<f32>(2.0 * inset, 2.0 * inset), vec2<f32>(0.0));
        let inner_local = i.local - vec2<f32>(inset, inset);
        let inner_radius = max(i.misc.x - inset, 0.0);
        let d_inner = rounded_rect_distance(inner_local, inner_size, inner_radius);
        let inner = 1.0 - smoothstep(-aa, aa, d_inner);
        coverage = outer * (1.0 - inner);
    }
    return vec4<f32>(i.color.rgb, i.color.a * coverage);
}

@fragment
fn fs_text(i: VertexOut) -> @location(0) vec4<f32> {
    let texel = textureSample(glyph_atlas, glyph_sampler, i.uv);
    if i.misc.z < 0.5 {
        // Mask glyph: RGB is supplied by the text style, atlas alpha is coverage.
        return vec4<f32>(i.color.rgb, i.color.a * texel.a);
    }
    // Color emoji/font glyph: preserve its RGB and multiply only its alpha by text opacity.
    return vec4<f32>(texel.rgb, texel.a * i.color.a);
}
"#;

const COPY_WGSL: &str = r#"
@group(0) @binding(0) var source_texture: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;
struct CopyOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};
@vertex
fn vs_copy(@builtin(vertex_index) index: u32) -> CopyOut {
    var positions = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
    var uvs = array<vec2<f32>, 3>(vec2<f32>(0.0, 1.0), vec2<f32>(2.0, 1.0), vec2<f32>(0.0, -1.0));
    var out: CopyOut;
    out.position = vec4<f32>(positions[index], 0.0, 1.0);
    out.uv = uvs[index];
    return out;
}
@fragment
fn fs_copy(i: CopyOut) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, i.uv);
}
fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let low = c / vec3<f32>(12.92);
    let high = pow((c + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4));
    return select(high, low, c <= vec3<f32>(0.04045));
}
@fragment
fn fs_copy_srgb(i: CopyOut) -> @location(0) vec4<f32> {
    let c = textureSample(source_texture, source_sampler, i.uv);
    return vec4<f32>(srgb_to_linear(c.rgb), c.a);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Vertex {
    position: [f32; 2],
    local: [f32; 2],
    size: [f32; 2],
    misc: [f32; 4],
    color: [f32; 4],
    uv: [f32; 2],
}

const VERTEX_ATTRIBUTES: [wgpu::VertexAttribute; 6] = wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x2, 3 => Float32x4, 4 => Float32x4, 5 => Float32x2];

struct Draw {
    range: std::ops::Range<u32>,
    text: bool,
}

#[derive(Clone, Copy)]
struct AtlasGlyph {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    left: i32,
    top: i32,
    color: bool,
}

struct WgpuRenderer {
    instance: wgpu::Instance,
    // Raw Wayland handles are retained until after the wgpu surface is dropped.
    surface: Option<wgpu::Surface<'static>>,
    _wl_surface: Option<WlSurface>,
    _connection: Option<Connection>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface_config: Option<wgpu::SurfaceConfiguration>,
    surface_format: Option<wgpu::TextureFormat>,
    target: wgpu::Texture,
    target_view: wgpu::TextureView,
    shape_pipeline: wgpu::RenderPipeline,
    text_pipeline: wgpu::RenderPipeline,
    present_layout: wgpu::BindGroupLayout,
    present_sampler: wgpu::Sampler,
    present_bind_group: Option<wgpu::BindGroup>,
    present_pipeline: Option<wgpu::RenderPipeline>,
    present_pipeline_srgb: Option<wgpu::RenderPipeline>,
    atlas: wgpu::Texture,
    atlas_bind_group: wgpu::BindGroup,
    fb: Vec<u8>,
    readback: bool,
    size: (i32, i32),
    fonts: Fonts,
    swash: SwashCache,
    glyphs: HashMap<CacheKey, AtlasGlyph>,
    atlas_x: u32,
    atlas_y: u32,
    atlas_row_height: u32,
    bg: Color,
}

impl WgpuRenderer {
    fn new(w: i32, h: i32, fonts: Fonts, bg: Color) -> Self {
        Self::build(
            w,
            h,
            fonts,
            bg,
            wgpu::Instance::default(),
            None,
            None,
            None,
            true,
        )
    }

    fn for_wayland(
        w: i32,
        h: i32,
        fonts: Fonts,
        bg: Color,
        conn: &Connection,
        wl_surface: &WlSurface,
    ) -> Self {
        use raw_window_handle::{
            RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
        };
        use std::ptr::NonNull;
        use wayland_client::Proxy;

        let instance = wgpu::Instance::default();
        let raw_display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            NonNull::new(conn.backend().display_ptr() as *mut _).expect("Wayland display pointer"),
        ));
        let raw_window = RawWindowHandle::Wayland(WaylandWindowHandle::new(
            NonNull::new(wl_surface.id().as_ptr() as *mut _).expect("Wayland surface pointer"),
        ));
        // SAFETY: the renderer stores a cloned Connection and WlSurface for the entire lifetime
        // of this surface, and drops the wgpu surface before those handles.
        let surface: wgpu::Surface<'static> = unsafe {
            instance.create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                raw_display_handle: raw_display,
                raw_window_handle: raw_window,
            })
        }
        .expect("wgpu: create Wayland surface");
        Self::build(
            w,
            h,
            fonts,
            bg,
            instance,
            Some(surface),
            Some(conn.clone()),
            Some(wl_surface.clone()),
            false,
        )
    }

    fn build(
        w: i32,
        h: i32,
        fonts: Fonts,
        bg: Color,
        instance: wgpu::Instance,
        surface: Option<wgpu::Surface<'static>>,
        connection: Option<Connection>,
        wl_surface: Option<WlSurface>,
        readback: bool,
    ) -> Self {
        let options = wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: surface.as_ref(),
            force_fallback_adapter: false,
        };
        let adapter = pollster::block_on(instance.request_adapter(&options))
            .or_else(|| {
                pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                    force_fallback_adapter: true,
                    ..options
                }))
            })
            .expect("wgpu: no compatible GPU/software adapter found");
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("ui-window device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
            },
            None,
        ))
        .expect("wgpu: request_device failed");

        let (surface_format, surface_config) = if let Some(surface) = surface.as_ref() {
            let caps = surface.get_capabilities(&adapter);
            let format = caps
                .formats
                .iter()
                .copied()
                .find(|f| {
                    matches!(
                        f,
                        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Rgba8Unorm
                    )
                })
                .or_else(|| caps.formats.iter().copied().find(|f| f.is_srgb()))
                .or_else(|| caps.formats.first().copied())
                .expect("wgpu: Wayland surface has no supported format");
            let alpha = caps
                .alpha_modes
                .iter()
                .copied()
                .find(|m| *m == wgpu::CompositeAlphaMode::Opaque)
                .or_else(|| caps.alpha_modes.first().copied())
                .expect("wgpu: Wayland surface has no alpha mode");
            (
                Some(format),
                Some(wgpu::SurfaceConfiguration {
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    format,
                    width: w.max(1) as u32,
                    height: h.max(1) as u32,
                    present_mode: wgpu::PresentMode::Fifo,
                    alpha_mode: alpha,
                    view_formats: vec![],
                    desired_maximum_frame_latency: 2,
                }),
            )
        } else {
            (None, None)
        };
        let atlas = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("cosmic-text glyph atlas"),
            size: wgpu::Extent3d {
                width: GLYPH_ATLAS_SIZE,
                height: GLYPH_ATLAS_SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let atlas_view = atlas.create_view(&wgpu::TextureViewDescriptor::default());
        let atlas_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glyph atlas sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let atlas_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("glyph atlas layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let atlas_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("glyph atlas bind group"),
            layout: &atlas_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&atlas_sampler),
                },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ui-window WGSL"),
            source: wgpu::ShaderSource::Wgsl(WGSL.into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("ui-window pipeline layout"),
            bind_group_layouts: &[&atlas_layout],
            push_constant_ranges: &[],
        });
        let vertex_layout_shape = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &VERTEX_ATTRIBUTES,
        };
        let vertex_layout_text = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &VERTEX_ATTRIBUTES,
        };
        let shape_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rounded rectangle pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[vertex_layout_shape],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_shape",
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });
        let text_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("text glyph pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[vertex_layout_text],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_text",
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        });

        let present_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("present blit layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let present_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("present blit sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let copy_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("fullscreen present shader"),
            source: wgpu::ShaderSource::Wgsl(COPY_WGSL.into()),
        });
        let copy_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("fullscreen present pipeline layout"),
            bind_group_layouts: &[&present_layout],
            push_constant_ranges: &[],
        });
        let present_pipeline = surface_format.map(|format| {
            Self::create_present_pipeline(
                &device,
                &copy_shader,
                &copy_pipeline_layout,
                format,
                false,
            )
        });
        let present_pipeline_srgb = surface_format.filter(|f| f.is_srgb()).map(|format| {
            Self::create_present_pipeline(
                &device,
                &copy_shader,
                &copy_pipeline_layout,
                format,
                true,
            )
        });

        let (target, target_view) = Self::make_target(&device, w, h);
        let present_bind_group = surface.as_ref().map(|_| {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("present source texture"),
                layout: &present_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&target_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&present_sampler),
                    },
                ],
            })
        });
        let mut renderer = WgpuRenderer {
            instance,
            surface,
            _wl_surface: wl_surface,
            _connection: connection,
            device,
            queue,
            surface_config,
            surface_format,
            target,
            target_view,
            shape_pipeline,
            text_pipeline,
            present_layout,
            present_sampler,
            present_bind_group,
            present_pipeline,
            present_pipeline_srgb,
            atlas,
            atlas_bind_group,
            fb: vec![0; w.max(1) as usize * h.max(1) as usize * 4],
            readback,
            size: (w.max(1), h.max(1)),
            fonts,
            swash: SwashCache::new(),
            glyphs: HashMap::new(),
            atlas_x: 0,
            atlas_y: 0,
            atlas_row_height: 0,
            bg,
        };
        renderer.fill_fb(bg);
        renderer.configure_surface();
        renderer
    }

    fn create_present_pipeline(
        device: &wgpu::Device,
        shader: &wgpu::ShaderModule,
        layout: &wgpu::PipelineLayout,
        format: wgpu::TextureFormat,
        convert_srgb: bool,
    ) -> wgpu::RenderPipeline {
        device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(if convert_srgb {
                "sRGB fullscreen blit pipeline"
            } else {
                "fullscreen blit pipeline"
            }),
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module: shader,
                entry_point: "vs_copy",
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: shader,
                entry_point: if convert_srgb {
                    "fs_copy_srgb"
                } else {
                    "fs_copy"
                },
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        })
    }

    fn configure_surface(&self) {
        if let (Some(surface), Some(config)) = (&self.surface, &self.surface_config) {
            surface.configure(&self.device, config);
        }
    }

    /// Present the complete persistent GPU target; swapchain contents are not persistent.
    fn present(&mut self) -> bool {
        if self.surface.is_none() {
            return false;
        }
        let frame = match self.surface.as_ref().unwrap().get_current_texture() {
            Ok(frame) => frame,
            Err(wgpu::SurfaceError::Lost) | Err(wgpu::SurfaceError::Outdated) => {
                self.configure_surface();
                match self.surface.as_ref().unwrap().get_current_texture() {
                    Ok(frame) => frame,
                    Err(wgpu::SurfaceError::Timeout) => return false,
                    Err(e) => panic!("wgpu: surface acquisition failed after reconfigure: {e}"),
                }
            }
            Err(wgpu::SurfaceError::Timeout) => return false,
            Err(wgpu::SurfaceError::OutOfMemory) => panic!("wgpu: surface out of memory"),
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("present encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("present blit pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            if self.surface_format.is_some_and(|f| f.is_srgb()) {
                pass.set_pipeline(
                    self.present_pipeline_srgb
                        .as_ref()
                        .expect("sRGB present pipeline"),
                );
            } else {
                pass.set_pipeline(self.present_pipeline.as_ref().expect("present pipeline"));
            }
            pass.set_bind_group(
                0,
                self.present_bind_group
                    .as_ref()
                    .expect("present bind group"),
                &[],
            );
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        true
    }

    fn make_target(device: &wgpu::Device, w: i32, h: i32) -> (wgpu::Texture, wgpu::TextureView) {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("persistent UI target"),
            size: wgpu::Extent3d {
                width: w.max(1) as u32,
                height: h.max(1) as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        (texture, view)
    }

    fn fill_fb(&mut self, c: Color) {
        for p in self.fb.chunks_exact_mut(4) {
            p.copy_from_slice(&[c.0, c.1, c.2, c.3]);
        }
    }

    fn color(c: Color) -> [f32; 4] {
        [
            c.0 as f32 / 255.0,
            c.1 as f32 / 255.0,
            c.2 as f32 / 255.0,
            c.3 as f32 / 255.0,
        ]
    }

    fn vertex(
        &self,
        x: f32,
        y: f32,
        local_x: f32,
        local_y: f32,
        w: f32,
        h: f32,
        misc: [f32; 4],
        color: [f32; 4],
        uv: [f32; 2],
    ) -> Vertex {
        let width = self.size.0.max(1) as f32;
        let height = self.size.1.max(1) as f32;
        Vertex {
            position: [x * 2.0 / width - 1.0, 1.0 - y * 2.0 / height],
            local: [local_x, local_y],
            size: [w, h],
            misc,
            color,
            uv,
        }
    }

    fn quad(
        &self,
        vertices: &mut Vec<Vertex>,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        misc: [f32; 4],
        color: [f32; 4],
        uv: [[f32; 2]; 4],
    ) {
        let a = self.vertex(x, y, 0.0, 0.0, w, h, misc, color, uv[0]);
        let b = self.vertex(x + w, y, w, 0.0, w, h, misc, color, uv[1]);
        let c = self.vertex(x + w, y + h, w, h, w, h, misc, color, uv[2]);
        let d = self.vertex(x, y + h, 0.0, h, w, h, misc, color, uv[3]);
        vertices.extend_from_slice(&[a, b, c, a, c, d]);
    }

    fn shape(
        &self,
        vertices: &mut Vec<Vertex>,
        r: Rect,
        radius: i32,
        width: i32,
        border: bool,
        color: Color,
    ) {
        if r.is_empty() {
            return;
        }
        self.quad(
            vertices,
            r.x as f32,
            r.y as f32,
            r.w as f32,
            r.h as f32,
            [
                radius.max(0) as f32,
                width.max(0) as f32,
                if border { 1.0 } else { 0.0 },
                0.0,
            ],
            Self::color(color),
            [[0.0, 0.0]; 4],
        );
    }

    fn ensure_glyph(
        &mut self,
        key: CacheKey,
        image: &cosmic_text::SwashImage,
    ) -> Option<AtlasGlyph> {
        if let Some(g) = self.glyphs.get(&key) {
            return Some(*g);
        }
        let (w, h) = (image.placement.width, image.placement.height);
        if w == 0 || h == 0 || w > GLYPH_ATLAS_SIZE || h > GLYPH_ATLAS_SIZE {
            return None;
        }
        if self.atlas_x + w > GLYPH_ATLAS_SIZE {
            self.atlas_x = 0;
            self.atlas_y += self.atlas_row_height;
            self.atlas_row_height = 0;
        }
        if self.atlas_y + h > GLYPH_ATLAS_SIZE {
            panic!("wgpu glyph atlas exhausted; increase GLYPH_ATLAS_SIZE or implement eviction");
        }
        let (x, y) = (self.atlas_x, self.atlas_y);
        self.atlas_x += w;
        self.atlas_row_height = self.atlas_row_height.max(h);

        let colored = match &image.content {
            SwashContent::Mask => false,
            SwashContent::Color => true,
            _ => return None,
        };
        let pixels = (w * h) as usize;
        let mut rgba = vec![0u8; pixels * 4];
        if colored {
            if image.data.len() < pixels * 4 {
                return None;
            }
            rgba.copy_from_slice(&image.data[..pixels * 4]);
        } else {
            if image.data.len() < pixels {
                return None;
            }
            for (i, coverage) in image.data[..pixels].iter().enumerate() {
                rgba[i * 4..i * 4 + 4].copy_from_slice(&[255, 255, 255, *coverage]);
            }
        }
        self.queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &self.atlas,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            &rgba,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let glyph = AtlasGlyph {
            x,
            y,
            width: w,
            height: h,
            left: image.placement.left,
            top: image.placement.top,
            color: colored,
        };
        self.glyphs.insert(key, glyph);
        Some(glyph)
    }

    fn append_text(
        &mut self,
        vertices: &mut Vec<Vertex>,
        x: i32,
        y: i32,
        color: Color,
        glyphs: &[Glyph],
        fs: &mut FontSystem,
    ) {
        let color = Self::color(color);
        for glyph in glyphs {
            if !self.glyphs.contains_key(&glyph.key) {
                let image = self.swash.get_image(fs, glyph.key).as_ref().cloned();
                if let Some(image) = image {
                    self.ensure_glyph(glyph.key, &image);
                }
            }
            let Some(atlas) = self.glyphs.get(&glyph.key).copied() else {
                continue;
            };
            let gx = x + glyph.x + atlas.left;
            let gy = y + glyph.y - atlas.top;
            let w = atlas.width as f32;
            let h = atlas.height as f32;
            let aw = GLYPH_ATLAS_SIZE as f32;
            let ah = GLYPH_ATLAS_SIZE as f32;
            let u0 = atlas.x as f32 / aw;
            let v0 = atlas.y as f32 / ah;
            let u1 = (atlas.x + atlas.width) as f32 / aw;
            let v1 = (atlas.y + atlas.height) as f32 / ah;
            self.quad(
                vertices,
                gx as f32,
                gy as f32,
                w,
                h,
                [0.0, 0.0, if atlas.color { 1.0 } else { 0.0 }, 0.0],
                color,
                [[u0, v0], [u1, v0], [u1, v1], [u0, v1]],
            );
        }
    }

    fn readback_damage(&mut self, d: Rect, mut encoder: wgpu::CommandEncoder) {
        let unpadded = d.w as u32 * 4;
        let padded = (unpadded + wgpu::COPY_BYTES_PER_ROW_ALIGNMENT - 1)
            / wgpu::COPY_BYTES_PER_ROW_ALIGNMENT
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let staging = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("wgpu damaged-pixels readback"),
            size: padded as u64 * d.h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d {
                    x: d.x as u32,
                    y: d.y as u32,
                    z: 0,
                },
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: &staging,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(d.h as u32),
                },
            },
            wgpu::Extent3d {
                width: d.w as u32,
                height: d.h as u32,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit(Some(encoder.finish()));
        let slice = staging.slice(..);
        let (sender, receiver) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device.poll(wgpu::Maintain::Wait);
        receiver
            .recv()
            .expect("wgpu map callback dropped")
            .expect("wgpu readback map failed");
        let mapped = slice.get_mapped_range();
        for row in 0..d.h as usize {
            let src = row * padded as usize;
            let dst = ((d.y as usize + row) * self.size.0 as usize + d.x as usize) * 4;
            self.fb[dst..dst + unpadded as usize]
                .copy_from_slice(&mapped[src..src + unpadded as usize]);
        }
        drop(mapped);
        staging.unmap();
    }

    fn save_png(&self, path: &str) {
        let file = std::fs::File::create(path).expect("create PNG");
        let mut encoder = png::Encoder::new(file, self.size.0 as u32, self.size.1 as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("write PNG header");
        writer.write_image_data(&self.fb).expect("write PNG pixels");
    }
}

impl Renderer for WgpuRenderer {
    fn resize(&mut self, w: i32, h: i32) {
        self.size = (w.max(1), h.max(1));
        let (target, view) = Self::make_target(&self.device, self.size.0, self.size.1);
        self.target = target;
        self.target_view = view;
        self.fb = vec![0; self.size.0 as usize * self.size.1 as usize * 4];
        self.fill_fb(self.bg);
        if let Some(config) = self.surface_config.as_mut() {
            config.width = self.size.0 as u32;
            config.height = self.size.1 as u32;
            self.configure_surface();
            self.present_bind_group =
                Some(self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("resized present source texture"),
                    layout: &self.present_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::TextureView(&self.target_view),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Sampler(&self.present_sampler),
                        },
                    ],
                }));
        }
    }

    fn render(&mut self, list: &[Cmd], damage: Rect) -> Rect {
        use wgpu::util::DeviceExt;
        let bounds = Rect {
            x: 0,
            y: 0,
            w: self.size.0,
            h: self.size.1,
        };
        let d = damage.intersect(bounds);
        if d.is_empty() {
            return d;
        }

        let mut vertices: Vec<Vertex> = Vec::new();
        let mut draws: Vec<Draw> = Vec::new();
        // Clear only the damaged region. The scissor keeps every other pixel persistent.
        let start = vertices.len() as u32;
        self.shape(&mut vertices, d, 0, 0, false, self.bg);
        for vertex in &mut vertices[start as usize..] {
            vertex.misc[2] = 2.0;
        }
        draws.push(Draw {
            range: start..vertices.len() as u32,
            text: false,
        });

        let fonts = self.fonts.clone();
        let mut fs = fonts.borrow_mut();
        for cmd in list {
            match cmd {
                Cmd::Fill { r, radius, color } => {
                    let start = vertices.len() as u32;
                    self.shape(&mut vertices, *r, *radius, 0, false, *color);
                    if vertices.len() as u32 > start {
                        draws.push(Draw {
                            range: start..vertices.len() as u32,
                            text: false,
                        });
                    }
                }
                Cmd::Border {
                    r,
                    radius,
                    width,
                    color,
                } => {
                    let start = vertices.len() as u32;
                    self.shape(&mut vertices, *r, *radius, *width, true, *color);
                    if vertices.len() as u32 > start {
                        draws.push(Draw {
                            range: start..vertices.len() as u32,
                            text: false,
                        });
                    }
                }
                Cmd::Text {
                    x,
                    y,
                    color,
                    glyphs,
                } => {
                    let start = vertices.len() as u32;
                    self.append_text(&mut vertices, *x, *y, *color, glyphs, &mut fs);
                    if vertices.len() as u32 > start {
                        draws.push(Draw {
                            range: start..vertices.len() as u32,
                            text: true,
                        });
                    }
                }
                #[cfg(feature = "paint-flash")]
                Cmd::Tint { rects, color } => {
                    let start = vertices.len() as u32;
                    for r in union_rects_nonoverlap(rects) {
                        self.shape(&mut vertices, r, 0, 0, false, *color);
                    }
                    if vertices.len() as u32 > start {
                        draws.push(Draw {
                            range: start..vertices.len() as u32,
                            text: false,
                        });
                    }
                }
            }
        }
        drop(fs);

        let vertex_buffer = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("ui-window frame vertices"),
                contents: bytemuck::cast_slice(&vertices),
                usage: wgpu::BufferUsages::VERTEX,
            });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ui-window frame encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("ui-window render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.target_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_viewport(0.0, 0.0, self.size.0 as f32, self.size.1 as f32, 0.0, 1.0);
            pass.set_scissor_rect(d.x as u32, d.y as u32, d.w as u32, d.h as u32);
            pass.set_vertex_buffer(0, vertex_buffer.slice(..));
            pass.set_bind_group(0, &self.atlas_bind_group, &[]);
            for draw in &draws {
                if draw.text {
                    pass.set_pipeline(&self.text_pipeline);
                } else {
                    pass.set_pipeline(&self.shape_pipeline);
                }
                pass.draw(draw.range.clone(), 0..1);
            }
        }
        if self.readback {
            self.readback_damage(d, encoder);
        } else {
            self.queue.submit(Some(encoder.finish()));
        }
        d
    }

    fn framebuffer(&self) -> &[u8] {
        &self.fb
    }
}

/// Split an integer rectangle union into disjoint horizontal strips. This is used by the
/// debug tint so intersecting updated containers do not accumulate alpha more than once.
#[cfg(feature = "paint-flash")]
fn union_rects_nonoverlap(rects: &[Rect]) -> Vec<Rect> {
    let mut ys = Vec::new();
    for r in rects.iter().filter(|r| !r.is_empty()) {
        ys.push(r.y);
        ys.push(r.y + r.h);
    }
    ys.sort_unstable();
    ys.dedup();
    let mut strips: Vec<Rect> = Vec::new();
    for band in ys.windows(2) {
        let (y0, y1) = (band[0], band[1]);
        if y0 == y1 {
            continue;
        }
        let mut xs: Vec<(i32, i32)> = rects
            .iter()
            .filter(|r| !r.is_empty() && r.y < y1 && r.y + r.h > y0)
            .map(|r| (r.x, r.x + r.w))
            .collect();
        xs.sort_unstable();
        let mut merged: Vec<(i32, i32)> = Vec::new();
        for (x0, x1) in xs {
            if let Some(last) = merged.last_mut() {
                if x0 <= last.1 {
                    last.1 = last.1.max(x1);
                    continue;
                }
            }
            merged.push((x0, x1));
        }
        for (x0, x1) in merged {
            if let Some(last) = strips
                .iter_mut()
                .rev()
                .find(|r| r.x == x0 && r.w == x1 - x0 && r.y + r.h == y0)
            {
                last.h += y1 - y0;
            } else {
                strips.push(Rect {
                    x: x0,
                    y: y0,
                    w: x1 - x0,
                    h: y1 - y0,
                });
            }
        }
    }
    strips
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
        self.layout(|l| (l.pad_x, l.pad_y) = (n, n))
    }
    fn pad_xy(self, x: i32, y: i32) -> Self {
        self.layout(|l| (l.pad_x, l.pad_y) = (x, y))
    }
    fn gap(self, n: i32) -> Self {
        self.layout(|l| l.gap = n)
    }
    fn border(self, width: i32) -> Self {
        self.layout(|l| l.border = width)
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
    fn center(self) -> Self {
        self.layout(|l| l.center = true)
    }
    fn fill(self, c: Color) -> Self {
        self.paint(|p| p.fill = Some(c))
    }
    fn radius(self, r: i32) -> Self {
        self.paint(|p| p.radius = r)
    }
    /// Text color.
    fn color(self, c: Color) -> Self {
        self.paint(|p| p.color = c)
    }
    fn child(self, c: impl Into<NodeId>) -> Self {
        self.cx.tree.borrow_mut().append(self.id, c.into());
        self
    }
    fn on_click(self, f: impl Fn() + 'static) -> Self {
        self.cx.tree.borrow_mut().set_click(self.id, Rc::new(f));
        self
    }
    fn on_hover(self, f: impl Fn(bool) + 'static) -> Self {
        self.cx.tree.borrow_mut().set_hover(self.id, Rc::new(f));
        self
    }
    /// Reactive binding: paint-only (never triggers layout).
    fn bind_fill(self, f: impl Fn() -> Option<Color> + 'static) -> Self {
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
    text_sz(cx, s, FONT_PX)
}
fn text_sz<'a>(cx: &'a Cx, s: &str, px: f32) -> V<'a> {
    let id = cx.tree.borrow_mut().create(Kind::Text);
    let mut t = cx.tree.borrow_mut();
    t.set_font(id, px);
    t.set_text(id, s.to_string());
    drop(t);
    V { cx, id }
}
fn text_dyn<'a>(cx: &'a Cx, f: impl Fn() -> String + 'static) -> V<'a> {
    text_dyn_sz(cx, f, FONT_PX)
}
fn text_dyn_sz<'a>(cx: &'a Cx, f: impl Fn() -> String + 'static, px: f32) -> V<'a> {
    let id = cx.tree.borrow_mut().create(Kind::Text);
    cx.tree.borrow_mut().set_font(id, px);
    bind(cx, id, f, |t, id, s| t.set_text(id, s));
    V { cx, id }
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
// 5. Ui: everything between "an event arrived" and "pixels are in the renderer".
//    Host-agnostic: the Wayland window and the headless PNG run drive the same type.
// ════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Copy)]
enum Event {
    Click(i32, i32),
    Move(i32, i32),
    Leave,
    Tick,
}

#[derive(Clone)]
struct Frame {
    damage: Rect,
    /// `(nodes synced, measure calls, live nodes)` when layout ran.
    layout: Option<(usize, usize, usize)>,
    cmds: usize,
}

struct Ui<R: Renderer> {
    rt: Runtime,
    root: OwnerHandle,
    tree: Rc<RefCell<Tree>>,
    woken: Rc<Cell<bool>>,
    renderer: R,
    size: (i32, i32),
    /// One entry per frame that updated something: its containers and when they stop flashing.
    #[cfg(feature = "paint-flash")]
    flashes: Vec<(Vec<Rect>, std::time::Instant)>,
}

#[cfg(feature = "paint-flash")]
const FLASH_LIFETIME: std::time::Duration = std::time::Duration::from_millis(450);
#[cfg(feature = "paint-flash")]
const FLASH_COLOR: Color = Color(255, 32, 32, 84); // translucent: the content stays readable

#[cfg(feature = "paint-flash")]
impl<R: Renderer> Ui<R> {
    /// Forget highlights whose time is up and damage their rects so they repaint clean.
    fn flash_expire(&mut self, now: std::time::Instant) {
        let mut t = self.tree.borrow_mut();
        self.flashes.retain(|(rects, until)| {
            if *until > now {
                return true;
            }
            for r in rects {
                t.push_damage(*r); // not `add_damage`: wiping must not flash again
            }
            false
        });
    }
    /// Make every live highlight due now (the next `frame` wipes them). For tests/headless.
    fn flash_expire_all(&mut self) {
        let now = std::time::Instant::now();
        for f in &mut self.flashes {
            f.1 = now;
        }
    }
    /// Let the highlights run out and repaint clean.
    fn flash_settle(&mut self) {
        self.flash_expire_all();
        self.frame();
    }
    /// The tint covering the live highlights that touch `area`, if any.
    fn flash_cmd_in(&self, area: Rect) -> Option<Cmd> {
        let rects: Vec<Rect> = self
            .flashes
            .iter()
            .flat_map(|f| f.0.iter())
            .filter(|r| r.intersects(&area))
            .copied()
            .collect();
        (!rects.is_empty()).then_some(Cmd::Tint {
            rects,
            color: FLASH_COLOR,
        })
    }
}

impl<R: Renderer> Ui<R> {
    /// `on_wake` runs inside the flush hook: it may only wake the host's loop (rule 5 of the
    /// runtime: the hook cannot read or mutate reactive state).
    fn new(
        size: (i32, i32),
        fonts: Fonts,
        renderer: R,
        app: impl FnOnce(&Cx) -> NodeId,
        mut on_wake: impl FnMut() + 'static,
    ) -> Self {
        let rt = Runtime::new();
        RT.with(|c| *c.borrow_mut() = Some(rt.clone()));

        let woken = Rc::new(Cell::new(false));
        let w2 = woken.clone();
        rt.set_request_flush_hook(move |_| {
            w2.set(true);
            on_wake();
        })
        .expect("hook");

        let tree = Rc::new(RefCell::new(Tree::new(fonts)));
        tree.borrow_mut().resize(size.0, size.1);
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

        Ui {
            rt,
            root,
            tree,
            woken,
            renderer,
            size,
            #[cfg(feature = "paint-flash")]
            flashes: vec![],
        }
    }

    fn dispatch_one(&self, ev: Event) {
        // Clone handlers out, release the tree borrow, THEN run user code.
        match ev {
            Event::Click(x, y) => {
                let h = self.tree.borrow().click_handler(x, y);
                if let Some(h) = h {
                    h();
                }
            }
            Event::Move(x, y) => {
                let hs = self.tree.borrow_mut().hover_move(Some((x, y)));
                for (h, entered) in hs {
                    h(entered);
                }
            }
            Event::Leave => {
                let hs = self.tree.borrow_mut().hover_move(None);
                for (h, entered) in hs {
                    h(entered);
                }
            }
            Event::Tick => {
                let h = self.tree.borrow().on_tick.clone();
                if let Some(h) = h {
                    h();
                }
            }
        }
    }

    /// Input. `batch()` only defers the host notification; writes are immediate and no
    /// effect runs here, so handlers cannot re-enter the tree.
    fn dispatch(&self, events: &[Event]) {
        self.rt
            .batch(|_| {
                for &ev in events {
                    self.dispatch_one(ev);
                }
                Ok(())
            })
            .expect("batch");
    }

    /// Reactive flush, only if the hook asked for it.
    fn pump(&mut self) -> Option<FlushReport> {
        self.woken
            .replace(false)
            .then(|| self.rt.flush(FlushOptions::default()).expect("flush"))
    }

    /// Layout (if needed) → paint the damaged region. `None` when nothing changed.
    fn frame(&mut self) -> Option<Frame> {
        #[cfg(feature = "paint-flash")]
        self.flash_expire(std::time::Instant::now());
        let (damage, layout, list) = {
            let mut t = self.tree.borrow_mut();
            if !t.needs_frame() {
                return None;
            }
            let layout = if t.dirty_layout {
                let (synced, measured) = t.layout(self.size.0, self.size.1);
                Some((synced, measured, t.live_nodes()))
            } else {
                None
            };
            let damage = t.take_damage()?;
            #[cfg_attr(not(feature = "paint-flash"), allow(unused_mut))]
            let mut list = t.paint(damage);
            #[cfg(feature = "paint-flash")]
            {
                // This frame's updated containers start flashing. Live highlights that the
                // repaint just wiped (they overlap the damage) are tinted again.
                let fresh = std::mem::take(&mut t.flash);
                if !fresh.is_empty() {
                    self.flashes
                        .push((fresh, std::time::Instant::now() + FLASH_LIFETIME));
                }
                list.extend(self.flash_cmd_in(damage));
            }
            (damage, layout, list)
        };
        let damage = self.renderer.render(&list, damage);
        Some(Frame {
            damage,
            layout,
            cmds: list.len(),
        })
    }

    fn resize(&mut self, w: i32, h: i32) {
        self.size = (w, h);
        self.tree.borrow_mut().resize(w, h);
        self.renderer.resize(w, h);
    }

    fn locate(&self, label: &str) -> (i32, i32) {
        let r = self
            .tree
            .borrow()
            .find_text(label)
            .unwrap_or_else(|| panic!("`{label}` not on screen"));
        (r.x + r.w / 2, r.y + r.h / 2)
    }

    fn shutdown(self) {
        self.rt.dispose(self.root).expect("dispose"); // runs cleanups: removes all nodes
        self.rt.shutdown().expect("shutdown");
        let left = self.tree.borrow().live_nodes();
        assert_eq!(left, 0, "UI nodes leaked");
        RT.with(|c| drop(c.borrow_mut().take()));
    }
}

// ════════════════════════════════════════════════════════════════════════════
// 6. Application: written once, in terms of signals and builders
// ════════════════════════════════════════════════════════════════════════════

use theme::*;

const TRACK_W: i32 = 240;

fn button<'a>(cx: &'a Cx, label: &str, f: impl Fn() + 'static) -> V<'a> {
    let hot = Sig::new(false); // owned by whoever is building: the root, a row, or a `show`
    div(cx)
        .pad_xy(18, 9)
        .radius(8)
        .bind_fill(move || Some(if hot.get() { ACCENT_HI } else { ACCENT })) // paint-only
        .on_hover(move |h| hot.set(h))
        .on_click(f)
        .child(text(cx, label).color(Color(255, 255, 255, 255)))
}

fn card<'a>(cx: &'a Cx) -> V<'a> {
    div(cx).pad(16).gap(8).radius(12).fill(SURFACE).border(1)
}

fn app(cx: &Cx) -> NodeId {
    let count = Sig::new(0i32);
    let items = Sig::new(vec![1u32, 2, 3]);
    let next_id = Sig::new(4u32);
    let progress = Sig::new(0i32);

    // Time-driven state: a tick handler (real animation would interpolate node props
    // directly and only publish start/end through signals).
    cx.tree.borrow_mut().on_tick = Some(Rc::new(move || {
        progress.update(|p| *p = (*p + 3) % (TRACK_W + 1))
    }));

    let is_even = move || count.get() % 2 == 0;

    div(cx)
        .pad(24)
        .gap(16)
        .fill(BG)
        .child(text_sz(cx, "reactive + wgpu", 24.0))
        .child(text(cx, "signals → dirty path → damage rect → pixels").color(MUTED))
        // Counter card: text size changes with the digits (layout), the bar only repaints.
        .child(
            card(cx)
                .layout(|l| l.dir = Dir::Row)
                .center()
                .gap(16)
                .child(text_dyn_sz(
                    cx,
                    move || format!("count = {}", count.get()),
                    28.0,
                ))
                .child(
                    div(cx)
                        .width(96)
                        .height(10)
                        .radius(5)
                        .bind_fill(move || Some(if count.get() % 2 == 0 { ACCENT } else { TRACK })),
                )
                .child(show(cx, is_even, |cx| {
                    div(cx)
                        .pad_xy(10, 3)
                        .radius(10)
                        .fill(GOOD)
                        .child(text(cx, "even").color(BG))
                        .id()
                })),
        )
        .child(
            row(cx)
                .gap(10)
                .child(button(cx, "+1", move || count.update(|n| *n += 1)))
                .child(button(cx, "-1", move || count.update(|n| *n -= 1)))
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
        // Keyed list: rows are built once; only their inner bindings re-run on `count`.
        .child(
            card(cx)
                .child(text(cx, "items").color(MUTED))
                .child(for_each(
                    cx,
                    move || items.get_cloned(),
                    move |cx, &id| {
                        row(cx)
                            .gap(12)
                            .child(text(cx, &format!("item {id}")))
                            .child(
                                text_dyn(cx, move || {
                                    format!("count × {id} = {}", count.get() * id as i32)
                                })
                                .color(MUTED),
                            )
                            .id()
                    },
                )),
        )
        // Geometry bound to a signal: only the bar's path is re-synced.
        .child(
            row(cx)
                .center()
                .gap(12)
                .child(text(cx, "progress").color(MUTED))
                .child(
                    div(cx)
                        .width(TRACK_W)
                        .height(10)
                        .radius(5)
                        .fill(TRACK)
                        .layout(|l| l.dir = Dir::Row)
                        .child(
                            div(cx)
                                .height(10)
                                .radius(5)
                                .fill(ACCENT)
                                .bind_width(move || progress.get()),
                        ),
                ),
        )
        .id()
}

const WIN_W: i32 = 460;
const WIN_H: i32 = 470;

fn new_fonts() -> Fonts {
    Rc::new(RefCell::new(FontSystem::new())) // loads the system fonts (fontconfig rules)
}

fn log_frame(label: &str, report: &Option<FlushReport>, frame: &Option<Frame>) {
    let flush = match report {
        Some(r) => format!("flush rounds={} effects={}", r.rounds, r.callbacks),
        None => "flush -".to_string(),
    };
    let fr = match frame {
        Some(f) => {
            let layout = match f.layout {
                Some((s, m, n)) => format!("layout synced={s}/{n} measured={m}"),
                None => "layout skipped".to_string(),
            };
            let d = f.damage;
            format!(
                "{layout} · damage {}x{}+{}+{} ({} px) cmds={}",
                d.w,
                d.h,
                d.x,
                d.y,
                d.w * d.h,
                f.cmds
            )
        }
        None => "frame skipped".to_string(),
    };
    println!("{label:<22} {flush:<28} {fr}");
}

// ════════════════════════════════════════════════════════════════════════════
// 7. Headless host: a scripted run that writes a PNG (and the same driver the tests use)
// ════════════════════════════════════════════════════════════════════════════

fn headless(png: &str) {
    let fonts = new_fonts();
    let renderer = WgpuRenderer::new(WIN_W, WIN_H, fonts.clone(), BG);
    let mut ui = Ui::new((WIN_W, WIN_H), fonts, renderer, app, || {});
    let step = |ui: &mut Ui<WgpuRenderer>, label: &str, events: Vec<Event>| {
        ui.dispatch(&events);
        let report = ui.pump();
        let frame = ui.frame();
        log_frame(label, &report, &frame);
    };
    step(&mut ui, "mount", vec![]);
    let plus = ui.locate("+1");
    step(&mut ui, "hover +1", vec![Event::Move(plus.0, plus.1)]);
    step(
        &mut ui,
        "click +1 x3",
        vec![Event::Click(plus.0, plus.1); 3],
    );
    let add = ui.locate("add");
    step(&mut ui, "click add", vec![Event::Click(add.0, add.1)]);
    #[cfg(feature = "paint-flash")]
    ui.flash_settle(); // as if time passed: the PNG then shows only what the last steps updated
    step(&mut ui, "20 ticks", vec![Event::Tick; 20]);
    step(&mut ui, "pointer leaves", vec![Event::Leave]);
    ui.renderer.save_png(png);
    println!("wrote {png}");
    ui.shutdown();
}

// ════════════════════════════════════════════════════════════════════════════
// 8. Wayland host: xdg_toplevel + direct wgpu surface, driven by calloop
//    Three event sources feed ONE loop and one state (`Wl`):
//      * the Wayland socket (WaylandSource)       → input, configure, and frame callbacks
//      * a calloop Ping woken by the flush hook   → Runtime::flush, then present
//      * a Timer                                  → Event::Tick (the progress animation)
//    Nothing renders from inside an input handler: handlers only write signals.
// ════════════════════════════════════════════════════════════════════════════

use std::time::Duration;

use calloop::{
    EventLoop,
    timer::{TimeoutAction, Timer},
};
use calloop_wayland_source::WaylandSource;
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum, delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_callback::{self, WlCallback},
        wl_compositor::WlCompositor,
        wl_pointer::{self, WlPointer},
        wl_registry::WlRegistry,
        wl_seat::{self, WlSeat},
        wl_surface::WlSurface,
    },
};
use wayland_protocols::xdg::shell::client::{
    xdg_surface::{self, XdgSurface},
    xdg_toplevel::{self, XdgToplevel},
    xdg_wm_base::{self, XdgWmBase},
};

const BTN_LEFT: u32 = 0x110;

struct Wl {
    conn: Connection,
    qh: QueueHandle<Wl>,
    surface: WlSurface,
    // Role objects must outlive the loop.
    _wm_base: XdgWmBase,
    _xdg: XdgSurface,
    _toplevel: XdgToplevel,
    pointer: Option<WlPointer>,

    ui: Ui<WgpuRenderer>,
    size: (i32, i32),
    pending_size: Option<(i32, i32)>,
    configured: bool,
    frame_pending: bool, // a wl_surface.frame callback is outstanding
    commit_damage: Rect, // rendered but not yet committed
    last_report: Option<FlushReport>,
    pos: (i32, i32),
    running: bool,
    log: bool,
    commits: u64,
    max_commits: Option<u64>,
}

impl Wl {
    /// The Ping fired: the flush hook asked for a flush. Flush, then try to present.
    fn wake(&mut self) {
        if let Some(r) = self.ui.pump() {
            self.last_report = Some(r);
        }
        self.present();
    }

    /// Reconfigure the GPU surface for a new size and repaint everything.
    fn resize(&mut self, w: i32, h: i32) {
        self.size = (w, h);
        self.ui.resize(w, h);
        self.commit_damage = Rect { x: 0, y: 0, w, h };
    }

    /// Render changed regions into the persistent GPU target, then present it through wgpu.
    /// Swapchain images are copied in full because they are not persistent between frames.
    fn present(&mut self) {
        if !self.configured || self.frame_pending {
            return;
        }
        if let Some(f) = self.ui.frame() {
            if self.log {
                log_frame("wayland", &self.last_report.take(), &Some(f.clone()));
            }
            self.commit_damage = self.commit_damage.union(f.damage);
        }
        if self.commit_damage.is_empty() {
            return;
        }
        // Queue the callback before wgpu commits the swapchain image to this wl_surface.
        self.surface.frame(&self.qh, ());
        if !self.ui.renderer.present() {
            return;
        }
        self.frame_pending = true;
        self.commit_damage = Rect::default();
        self.commits += 1;
        if self.max_commits.is_some_and(|m| self.commits >= m) {
            self.running = false;
        }
    }
}

// ── protocol plumbing ──────────────────────────────────────────────────────

impl Dispatch<WlRegistry, GlobalListContents> for Wl {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as wayland_client::Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
delegate_noop!(Wl: ignore WlCompositor);
delegate_noop!(Wl: ignore WlSurface);

impl Dispatch<XdgWmBase, ()> for Wl {
    fn event(
        _: &mut Self,
        wm: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial); // the compositor checks that we are alive
        }
    }
}

impl Dispatch<XdgSurface, ()> for Wl {
    fn event(
        state: &mut Self,
        xdg: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg.ack_configure(serial);
            let (w, h) = state.pending_size.take().unwrap_or(state.size);
            if !state.configured || (w, h) != state.size {
                state.resize(w, h);
            }
            state.configured = true;
            state.present(); // the first attach after the first configure maps the window
        }
    }
}

impl Dispatch<XdgToplevel, ()> for Wl {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // 0×0 means "pick your own size".
            xdg_toplevel::Event::Configure { width, height, .. } if width > 0 && height > 0 => {
                state.pending_size = Some((width, height));
            }
            xdg_toplevel::Event::Close => state.running = false,
            _ => {}
        }
    }
}

impl Dispatch<WlCallback, ()> for Wl {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.frame_pending = false;
            state.present(); // whatever accumulated while we waited goes out now
        }
    }
}

impl Dispatch<WlSeat, ()> for Wl {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
            && caps.contains(wl_seat::Capability::Pointer)
            && state.pointer.is_none()
        {
            state.pointer = Some(seat.get_pointer(qh, ()));
        }
    }
}

impl Dispatch<WlPointer, ()> for Wl {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Handlers only write signals (inside `batch`); the flush hook's Ping does the rest.
        match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            }
            | wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                state.pos = (surface_x as i32, surface_y as i32);
                state.ui.dispatch(&[Event::Move(state.pos.0, state.pos.1)]);
            }
            wl_pointer::Event::Leave { .. } => state.ui.dispatch(&[Event::Leave]),
            wl_pointer::Event::Button {
                button,
                state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                ..
            } if button == BTN_LEFT => {
                state.ui.dispatch(&[Event::Click(state.pos.0, state.pos.1)]);
            }
            _ => {}
        }
    }
}

fn run_window(max_commits: Option<u64>, animate: bool) {
    let conn =
        Connection::connect_to_env().expect("no Wayland compositor (is WAYLAND_DISPLAY set?)");
    let (globals, queue) = registry_queue_init::<Wl>(&conn).expect("registry");
    let qh = queue.handle();

    let compositor: WlCompositor = globals.bind(&qh, 1..=6, ()).expect("wl_compositor");
    let wm_base: XdgWmBase = globals.bind(&qh, 1..=6, ()).expect("xdg_wm_base");
    let _seat: Option<WlSeat> = globals.bind(&qh, 1..=7, ()).ok(); // no seat → no input, still fine

    let surface = compositor.create_surface(&qh, ());
    let xdg = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title("reactive + wgpu".into());
    toplevel.set_app_id("reactive.ui-window".into());
    toplevel.set_min_size(320, 300);
    surface.commit(); // role assigned, no buffer yet → the compositor answers with configure

    let mut event_loop: EventLoop<Wl> = EventLoop::try_new().expect("event loop");
    let handle = event_loop.handle();

    // The flush hook may only wake the loop: Ping is an eventfd.
    let (ping, ping_source) = calloop::ping::make_ping().expect("ping");
    let fonts = new_fonts();
    let renderer = WgpuRenderer::for_wayland(WIN_W, WIN_H, fonts.clone(), BG, &conn, &surface);
    let ui = Ui::new((WIN_W, WIN_H), fonts, renderer, app, move || ping.ping());

    let mut wl = Wl {
        conn: conn.clone(),
        qh,
        surface,
        _wm_base: wm_base,
        _xdg: xdg,
        _toplevel: toplevel,
        pointer: None,
        ui,
        size: (WIN_W, WIN_H),
        pending_size: None,
        configured: false,
        frame_pending: false,
        commit_damage: Rect::default(),
        last_report: None,
        pos: (0, 0),
        running: true,
        log: std::env::var_os("UI_LOG").is_some(),
        commits: 0,
        max_commits,
    };

    handle
        .insert_source(ping_source, |_, _, wl| wl.wake())
        .expect("ping source");
    if animate {
        let tick = Duration::from_millis(33);
        handle
            .insert_source(Timer::from_duration(tick), move |_, _, wl| {
                wl.ui.dispatch(&[Event::Tick]); // writes a signal; the hook's Ping follows
                TimeoutAction::ToDuration(tick)
            })
            .expect("timer");
    }
    // Highlights must be wiped after their lifetime even when nothing else happens, so poll.
    // `present` is a no-op unless a highlight is due. Debug feature only.
    #[cfg(feature = "paint-flash")]
    {
        let poll = Duration::from_millis(50);
        handle
            .insert_source(Timer::from_duration(poll), move |_, _, wl| {
                wl.present();
                TimeoutAction::ToDuration(poll)
            })
            .expect("flash timer");
    }
    WaylandSource::new(conn.clone(), queue)
        .insert(handle)
        .expect("wayland source");

    while wl.running {
        event_loop.dispatch(None, &mut wl).expect("dispatch");
    }
    conn.flush().ok();
    wl.ui.shutdown();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
    };
    if args.iter().any(|a| a == "--png") {
        headless(value("--png").map_or("ui.png", |s| s.as_str()));
    } else {
        // --frames N: exit after N commits (CI / smoke tests); --static: no animation timer.
        let frames = value("--frames").and_then(|s| s.parse().ok());
        run_window(frames, !args.iter().any(|a| a == "--static"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(mut seed: u32) -> impl FnMut() -> u32 {
        move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        }
    }

    /// Only the container that was updated turns red; everything else is untouched.
    #[cfg(feature = "paint-flash")]
    #[test]
    fn flash_highlights_only_what_updated() {
        let fonts = new_fonts();
        let renderer = WgpuRenderer::new(WIN_W, WIN_H, fonts.clone(), BG);
        let mut ui = Ui::new((WIN_W, WIN_H), fonts, renderer, app, || {});
        ui.pump(); // run the mount effects
        ui.frame();
        ui.flash_settle();
        let before = ui.renderer.framebuffer().to_vec();

        let (x, y) = ui.locate("+1");
        let btn = {
            let t = ui.tree.borrow();
            t.abs_rect(t.bubble(x, y, |n| n.on_hover.is_some()).expect("button"))
        };
        ui.dispatch(&[Event::Move(x, y)]); // hover: the button's fill changes
        ui.pump();
        ui.frame();
        let after = ui.renderer.framebuffer();

        let mut changed = 0;
        for p in 0..(WIN_W * WIN_H) as usize {
            if after[p * 4..p * 4 + 4] != before[p * 4..p * 4 + 4] {
                changed += 1;
                let (px, py) = (p as i32 % WIN_W, p as i32 / WIN_W);
                assert!(
                    btn.contains(px, py),
                    "pixel ({px},{py}) changed outside the updated container {btn:?}"
                );
            }
        }
        assert!(changed > 0);
        // A corner-ish pixel of the button (padding, no glyph): hover fill ACCENT_HI is
        // (0x7c, 0x96, 0xff); with the tint on top, red must be clearly higher.
        let o = ((btn.y + 4) * WIN_W + btn.x + 8) as usize * 4;
        assert!(after[o] > 0x7c + 20, "tint missing: r = {}", after[o]);
        ui.shutdown();
    }

    /// (`FUZZ_SEED=n cargo test …` picks another sequence.)
    /// 300 pseudo-random interactions (1-3 events per loop turn). After every frame the
    /// incrementally synced rects must equal a from-scratch read of Taffy's layout, and the
    /// damage-only framebuffer must equal a full repaint rendered from scratch, so a stale
    /// rect or a missed damage region anywhere fails the test.
    #[test]
    fn incremental_layout_and_damage_match_full_recompute() {
        let fonts = new_fonts();
        let renderer = WgpuRenderer::new(WIN_W, WIN_H, fonts.clone(), BG);
        let mut ui = Ui::new((WIN_W, WIN_H), fonts.clone(), renderer, app, || {});
        let mut reference = WgpuRenderer::new(WIN_W, WIN_H, fonts, BG);
        let mut next = rng(std::env::var("FUZZ_SEED")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0x2545_F491));
        let mut worst = 0;
        for i in 0..300 {
            let events: Vec<Event> = if i == 0 {
                vec![]
            } else {
                (0..=next() % 3)
                    .map(|_| {
                        let click = |ui: &Ui<WgpuRenderer>, l| {
                            let (x, y) = ui.locate(l);
                            Event::Click(x, y)
                        };
                        match next() % 8 {
                            0 => click(&ui, "+1"),
                            1 => click(&ui, "-1"),
                            2 => click(&ui, "add"),
                            3 => click(&ui, "pop"),
                            4 => Event::Tick,
                            5 => {
                                let (x, y) =
                                    ui.locate(["+1", "-1", "add", "pop"][(next() % 4) as usize]);
                                Event::Move(x, y)
                            }
                            6 => Event::Leave,
                            _ => Event::Click(1, 1),
                        }
                    })
                    .collect()
            };
            ui.dispatch(&events);
            ui.pump();
            #[cfg(feature = "paint-flash")]
            if next().is_multiple_of(12) {
                ui.flash_expire_all(); // simulate time passing: the wipe must leave no residue
            }
            ui.frame();
            ui.tree.borrow().verify_rects();

            let full = Rect {
                x: 0,
                y: 0,
                w: WIN_W,
                h: WIN_H,
            };
            #[cfg_attr(not(feature = "paint-flash"), allow(unused_mut))]
            let mut list = ui.tree.borrow().paint(full);
            // With the feature on, the expected picture is "clean content + tint of exactly the
            // live highlights": anything else (residue, missing or double tint) fails.
            #[cfg(feature = "paint-flash")]
            list.extend(ui.flash_cmd_in(full));
            reference.render(&list, full);
            // Signed-distance-field antialiasing is independent of the scissor edge, so incremental
            // and full renders should be equivalent up to GPU blend/UNORM rounding.
            let max_diff = ui
                .renderer
                .framebuffer()
                .iter()
                .zip(reference.framebuffer())
                .map(|(a, b)| a.abs_diff(*b))
                .max()
                .unwrap_or(0);
            assert!(
                max_diff <= 2,
                "damage-only framebuffer differs from a full repaint at step {i}: max diff {max_diff}"
            );
            worst = worst.max(max_diff);
        }
        eprintln!("worst channel difference over 300 frames: {worst}");
        ui.shutdown();
    }
}
