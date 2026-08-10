//! Throwaway lineage layout prototype.
//!
//! This file exists only to make the proposal's geometry and golden frames
//! executable before production work starts. It is deliberately `std`-only
//! and is **not** a copy source for `crates/ab-tui`; production code must be
//! rewritten there with ratatui types, repository error handling, and tests.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

const BOX_W: usize = 22;
const BOX_H: usize = 5;
const COL_GAP: usize = 8;
const ROW_GAP: usize = 1;
const ROOT_GAP: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EdgeKind {
    Spawn,
    Relay,
    Unknown,
}

impl EdgeKind {
    fn compact(self) -> &'static str {
        match self {
            Self::Spawn => "──▸ ",
            Self::Relay => "══▸ ",
            Self::Unknown => "··?▸ ",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NodeKind {
    Generation,
    Gap,
    Tombstone,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    id: &'static str,
    parent: Option<&'static str>,
    edge: Option<EdgeKind>,
    kind: NodeKind,
    name: &'static str,
    meta: &'static str,
    status: &'static str,
    selected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LineageProjection {
    nodes: Vec<Node>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Rect {
    fn right(self) -> usize {
        self.x + self.w - 1
    }

    fn bottom(self) -> usize {
        self.y + self.h - 1
    }

    fn center_y(self) -> usize {
        self.y + self.h / 2
    }

    fn intersects(self, other: Self) -> bool {
        self.x <= other.right()
            && other.x <= self.right()
            && self.y <= other.bottom()
            && other.y <= self.bottom()
    }

    fn contains_strict(self, x: usize, y: usize) -> bool {
        x > self.x && x < self.right() && y > self.y && y < self.bottom()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NodeRect {
    id: &'static str,
    rect: Rect,
    kind: NodeKind,
    selected: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SegmentStyle {
    Neutral,
    Spawn,
    Relay,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SegmentAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct EdgeSegment {
    from: (usize, usize),
    to: (usize, usize),
    axis: SegmentAxis,
    style: SegmentStyle,
    terminal: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Bounds {
    w: usize,
    h: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Band {
    root: &'static str,
    top: usize,
    bottom: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SceneGeometry {
    node_rects: Vec<NodeRect>,
    edge_segments: Vec<EdgeSegment>,
    selectable_order: Vec<&'static str>,
    canvas_bounds: Bounds,
    bands: Vec<Band>,
}

fn fixture() -> LineageProjection {
    use EdgeKind::{Relay, Spawn, Unknown};
    use NodeKind::{Gap, Generation, Manual, Tombstone};

    LineageProjection {
        nodes: vec![
            Node {
                id: "g-root",
                parent: None,
                edge: None,
                kind: Generation,
                name: "p4w01",
                meta: "[planner] · codex",
                status: "running 12m",
                selected: false,
            },
            Node {
                id: "g-relay-1",
                parent: Some("g-root"),
                edge: Some(Spawn),
                kind: Generation,
                name: "p4w05",
                meta: "[executor] · claude",
                status: "running 5m",
                selected: false,
            },
            Node {
                id: "g-arch-1",
                parent: Some("g-relay-1"),
                edge: Some(Relay),
                kind: Generation,
                name: "p4w02",
                meta: "— · codex",
                status: "archived · evicted",
                selected: false,
            },
            Node {
                id: "g-arch-2",
                parent: Some("g-arch-1"),
                edge: Some(Relay),
                kind: Generation,
                name: "p4w03",
                meta: "— · claude",
                status: "archived · despawned",
                selected: false,
            },
            Node {
                id: "g-selected",
                parent: Some("g-arch-2"),
                edge: Some(Relay),
                kind: Generation,
                name: "p4w04",
                meta: "[tester] · agy",
                status: "running 2m",
                selected: true,
            },
            Node {
                id: "g-blocked",
                parent: Some("g-root"),
                edge: Some(Spawn),
                kind: Generation,
                name: "p4w06",
                meta: "[executor] · claude",
                status: "running 8m ⛔",
                selected: false,
            },
            Node {
                id: "gap",
                parent: None,
                edge: None,
                kind: Gap,
                name: "… unknown generations",
                meta: "",
                status: "",
                selected: false,
            },
            Node {
                id: "missing",
                parent: Some("gap"),
                edge: Some(Unknown),
                kind: Tombstone,
                name: "† …b8ef missing",
                meta: "",
                status: "",
                selected: false,
            },
            Node {
                id: "g-partial",
                parent: Some("missing"),
                edge: Some(Unknown),
                kind: Generation,
                name: "p4w07",
                meta: "— · codex",
                status: "idle 3m",
                selected: false,
            },
            Node {
                id: "manual",
                parent: None,
                edge: None,
                kind: Manual,
                name: "p4w09",
                meta: "— · claude",
                status: "manual · not selectable",
                selected: false,
            },
        ],
    }
}

fn node_height(kind: NodeKind) -> usize {
    match kind {
        NodeKind::Generation | NodeKind::Manual => BOX_H,
        NodeKind::Gap | NodeKind::Tombstone => 1,
    }
}

fn build_index(projection: &LineageProjection) -> Result<HashMap<&'static str, usize>, String> {
    let mut index = HashMap::new();
    for (i, node) in projection.nodes.iter().enumerate() {
        if index.insert(node.id, i).is_some() {
            return Err(format!("duplicate node id: {}", node.id));
        }
    }
    Ok(index)
}

fn children_of(
    projection: &LineageProjection,
    index: &HashMap<&'static str, usize>,
) -> Result<Vec<Vec<usize>>, String> {
    let mut children = vec![Vec::new(); projection.nodes.len()];
    for (i, node) in projection.nodes.iter().enumerate() {
        if let Some(parent) = node.parent {
            let Some(&p) = index.get(parent) else {
                return Err(format!("missing parent {parent} for {}", node.id));
            };
            children[p].push(i);
        }
    }
    Ok(children)
}

fn compute_span(
    i: usize,
    children: &[Vec<usize>],
    marks: &mut [u8],
    spans: &mut [usize],
) -> Result<usize, String> {
    match marks[i] {
        1 => return Err("cycle in lineage projection".to_string()),
        2 => return Ok(spans[i]),
        _ => {}
    }
    marks[i] = 1;
    let mut total = 0;
    for &child in &children[i] {
        total += compute_span(child, children, marks, spans)?;
    }
    spans[i] = total.max(1);
    marks[i] = 2;
    Ok(spans[i])
}

struct Placement<'a> {
    projection: &'a LineageProjection,
    children: &'a [Vec<usize>],
    spans: &'a [usize],
    rects: &'a mut [Option<Rect>],
}

impl Placement<'_> {
    fn assign(&mut self, i: usize, depth: usize, first_slot: usize, band_top: usize) {
        let span = self.spans[i];
        let first_center = band_top + BOX_H / 2 + first_slot * (BOX_H + ROW_GAP);
        let last_center = band_top + BOX_H / 2 + (first_slot + span - 1) * (BOX_H + ROW_GAP);
        let center = (first_center + last_center) / 2;
        let h = node_height(self.projection.nodes[i].kind);
        self.rects[i] = Some(Rect {
            x: depth * (BOX_W + COL_GAP),
            y: center.saturating_sub(h / 2),
            w: BOX_W,
            h,
        });

        let mut slot = first_slot;
        for &child in &self.children[i] {
            self.assign(child, depth + 1, slot, band_top);
            slot += self.spans[child];
        }
    }
}

fn segment_style(kind: EdgeKind) -> SegmentStyle {
    match kind {
        EdgeKind::Spawn => SegmentStyle::Spawn,
        EdgeKind::Relay => SegmentStyle::Relay,
        EdgeKind::Unknown => SegmentStyle::Unknown,
    }
}

fn flow_layout(projection: &LineageProjection) -> Result<SceneGeometry, String> {
    let index = build_index(projection)?;
    let children = children_of(projection, &index)?;
    let roots: Vec<usize> = projection
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(i, n)| n.parent.is_none().then_some(i))
        .collect();
    if roots.is_empty() && !projection.nodes.is_empty() {
        return Err("projection has nodes but no root".to_string());
    }

    let mut marks = vec![0; projection.nodes.len()];
    let mut spans = vec![0; projection.nodes.len()];
    for &root in &roots {
        compute_span(root, &children, &mut marks, &mut spans)?;
    }
    if marks.iter().any(|mark| *mark != 2) {
        return Err("projection contains an unreachable cycle".to_string());
    }

    let mut rects = vec![None; projection.nodes.len()];
    let mut bands = Vec::new();
    let mut band_top = 0;
    for &root in &roots {
        Placement {
            projection,
            children: &children,
            spans: &spans,
            rects: &mut rects,
        }
        .assign(root, 0, 0, band_top);

        let mut stack = vec![root];
        let mut band_bottom = band_top;
        while let Some(i) = stack.pop() {
            let rect = rects[i].expect("assigned subtree node");
            band_bottom = band_bottom.max(rect.bottom());
            stack.extend(children[i].iter().copied());
        }
        bands.push(Band {
            root: projection.nodes[root].id,
            top: band_top,
            bottom: band_bottom,
        });
        band_top = band_bottom + 1 + ROOT_GAP;
    }

    let node_rects: Vec<NodeRect> = projection
        .nodes
        .iter()
        .enumerate()
        .map(|(i, node)| NodeRect {
            id: node.id,
            rect: rects[i].expect("all nodes assigned"),
            kind: node.kind,
            selected: node.selected,
        })
        .collect();

    let mut edge_segments = Vec::new();
    for (parent, child_list) in children.iter().enumerate() {
        if child_list.is_empty() {
            continue;
        }
        let parent_rect = rects[parent].expect("parent rect");
        let parent_y = parent_rect.center_y();
        let trunk_x = parent_rect.right() + COL_GAP / 2;
        edge_segments.push(EdgeSegment {
            from: (parent_rect.right() + 1, parent_y),
            to: (trunk_x, parent_y),
            axis: SegmentAxis::Horizontal,
            style: SegmentStyle::Neutral,
            terminal: false,
        });
        let min_y = child_list
            .iter()
            .map(|i| rects[*i].expect("child rect").center_y())
            .min()
            .unwrap_or(parent_y)
            .min(parent_y);
        let max_y = child_list
            .iter()
            .map(|i| rects[*i].expect("child rect").center_y())
            .max()
            .unwrap_or(parent_y)
            .max(parent_y);
        edge_segments.push(EdgeSegment {
            from: (trunk_x, min_y),
            to: (trunk_x, max_y),
            axis: SegmentAxis::Vertical,
            style: SegmentStyle::Neutral,
            terminal: false,
        });
        for &child in child_list {
            let child_rect = rects[child].expect("child rect");
            let kind = projection.nodes[child]
                .edge
                .ok_or_else(|| format!("child {} has no edge kind", projection.nodes[child].id))?;
            edge_segments.push(EdgeSegment {
                from: (trunk_x + 1, child_rect.center_y()),
                to: (child_rect.x - 1, child_rect.center_y()),
                axis: SegmentAxis::Horizontal,
                style: segment_style(kind),
                terminal: true,
            });
        }
    }

    fn dfs(
        i: usize,
        projection: &LineageProjection,
        children: &[Vec<usize>],
        out: &mut Vec<&'static str>,
    ) {
        if projection.nodes[i].kind == NodeKind::Generation {
            out.push(projection.nodes[i].id);
        }
        for &child in &children[i] {
            dfs(child, projection, children, out);
        }
    }
    let mut selectable_order = Vec::new();
    for &root in &roots {
        dfs(root, projection, &children, &mut selectable_order);
    }

    let canvas_bounds = Bounds {
        w: node_rects
            .iter()
            .map(|n| n.rect.right() + 1)
            .max()
            .unwrap_or(0),
        h: node_rects
            .iter()
            .map(|n| n.rect.bottom() + 1)
            .max()
            .unwrap_or(0),
    };

    Ok(SceneGeometry {
        node_rects,
        edge_segments,
        selectable_order,
        canvas_bounds,
        bands,
    })
}

fn segment_points(segment: &EdgeSegment) -> Vec<(usize, usize)> {
    match segment.axis {
        SegmentAxis::Horizontal => {
            let (a, b) = if segment.from.0 <= segment.to.0 {
                (segment.from.0, segment.to.0)
            } else {
                (segment.to.0, segment.from.0)
            };
            (a..=b).map(|x| (x, segment.from.1)).collect()
        }
        SegmentAxis::Vertical => {
            let (a, b) = if segment.from.1 <= segment.to.1 {
                (segment.from.1, segment.to.1)
            } else {
                (segment.to.1, segment.from.1)
            };
            (a..=b).map(|y| (segment.from.0, y)).collect()
        }
    }
}

fn validate_geometry(projection: &LineageProjection, scene: &SceneGeometry) -> Result<(), String> {
    for (i, a) in scene.node_rects.iter().enumerate() {
        for b in scene.node_rects.iter().skip(i + 1) {
            if a.rect.intersects(b.rect) {
                return Err(format!("overlap: {} and {}", a.id, b.id));
            }
        }
    }
    for node in &projection.nodes {
        let Some(parent) = node.parent else { continue };
        let p = scene
            .node_rects
            .iter()
            .find(|r| r.id == parent)
            .ok_or_else(|| format!("missing parent rect: {parent}"))?;
        let c = scene
            .node_rects
            .iter()
            .find(|r| r.id == node.id)
            .ok_or_else(|| format!("missing child rect: {}", node.id))?;
        if p.rect.x >= c.rect.x {
            return Err(format!(
                "parent is not left of child: {parent} -> {}",
                node.id
            ));
        }
    }
    for pair in scene.bands.windows(2) {
        if pair[0].bottom >= pair[1].top {
            return Err(format!(
                "bands overlap: {} and {}",
                pair[0].root, pair[1].root
            ));
        }
    }
    for segment in &scene.edge_segments {
        for (x, y) in segment_points(segment) {
            if let Some(node) = scene
                .node_rects
                .iter()
                .find(|node| node.rect.contains_strict(x, y))
            {
                return Err(format!("edge crosses node interior: {}", node.id));
            }
        }
    }
    let expected = [
        "g-root",
        "g-relay-1",
        "g-arch-1",
        "g-arch-2",
        "g-selected",
        "g-blocked",
        "g-partial",
    ];
    if scene.selectable_order != expected {
        return Err(format!(
            "unexpected DFS order: {:?}",
            scene.selectable_order
        ));
    }
    Ok(())
}

#[derive(Clone)]
struct Canvas {
    width: usize,
    height: usize,
    cells: Vec<Vec<char>>,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![vec![' '; width]; height],
        }
    }

    fn put(&mut self, x: usize, y: usize, ch: char) {
        if x < self.width && y < self.height {
            self.cells[y][x] = ch;
        }
    }

    fn text(&mut self, x: usize, y: usize, text: &str, max: usize) {
        for (dx, ch) in text.chars().take(max).enumerate() {
            self.put(x + dx, y, ch);
        }
    }

    fn horizontal(&mut self, x1: usize, x2: usize, y: usize, ch: char) {
        for x in x1.min(x2)..=x1.max(x2) {
            self.put(x, y, ch);
        }
    }

    fn vertical(&mut self, x: usize, y1: usize, y2: usize, ch: char) {
        for y in y1.min(y2)..=y1.max(y2) {
            self.put(x, y, ch);
        }
    }

    fn into_string(self) -> String {
        let mut out = String::new();
        for row in self.cells {
            for ch in row {
                out.push(ch);
            }
            out.push('\n');
        }
        out
    }
}

fn draw_segment(canvas: &mut Canvas, segment: &EdgeSegment) {
    let ch = match (segment.axis, segment.style) {
        (SegmentAxis::Vertical, _) => '│',
        (SegmentAxis::Horizontal, SegmentStyle::Neutral | SegmentStyle::Spawn) => '─',
        (SegmentAxis::Horizontal, SegmentStyle::Relay) => '═',
        (SegmentAxis::Horizontal, SegmentStyle::Unknown) => '·',
    };
    match segment.axis {
        SegmentAxis::Horizontal => {
            canvas.horizontal(segment.from.0, segment.to.0, segment.from.1, ch)
        }
        SegmentAxis::Vertical => canvas.vertical(segment.from.0, segment.from.1, segment.to.1, ch),
    }
    if segment.terminal {
        if segment.style == SegmentStyle::Unknown && segment.to.0 > segment.from.0 {
            canvas.put(segment.to.0.saturating_sub(1), segment.to.1, '?');
        }
        canvas.put(segment.to.0, segment.to.1, '▸');
    }
}

fn draw_box(canvas: &mut Canvas, rect: Rect, node: &Node) {
    if matches!(node.kind, NodeKind::Gap | NodeKind::Tombstone) {
        canvas.text(rect.x, rect.y, node.name, rect.w);
        return;
    }
    canvas.put(rect.x, rect.y, '┌');
    canvas.horizontal(rect.x + 1, rect.right() - 1, rect.y, '─');
    canvas.put(rect.right(), rect.y, '┐');
    canvas.put(rect.x, rect.bottom(), '└');
    canvas.horizontal(rect.x + 1, rect.right() - 1, rect.bottom(), '─');
    canvas.put(rect.right(), rect.bottom(), '┘');
    for y in rect.y + 1..rect.bottom() {
        canvas.put(rect.x, y, '│');
        canvas.put(rect.right(), y, '│');
    }
    let marker = if node.selected { "▶" } else { " " };
    canvas.text(
        rect.x + 1,
        rect.y + 1,
        &format!("{marker}{}", node.name),
        rect.w - 2,
    );
    canvas.text(rect.x + 1, rect.y + 2, node.meta, rect.w - 2);
    canvas.text(rect.x + 1, rect.y + 3, node.status, rect.w - 2);
}

fn camera(scene: &SceneGeometry, viewport: Bounds) -> (usize, usize) {
    let selected = scene
        .node_rects
        .iter()
        .find(|node| node.selected)
        .or_else(|| scene.node_rects.first());
    let Some(selected) = selected else {
        return (0, 0);
    };
    let x = (selected.rect.right() + 1).saturating_sub(viewport.w);
    let y = (selected.rect.bottom() + 1).saturating_sub(viewport.h);
    (
        x.min(scene.canvas_bounds.w.saturating_sub(viewport.w)),
        y.min(scene.canvas_bounds.h.saturating_sub(viewport.h)),
    )
}

fn copy_viewport(canvas: &Canvas, frame: &mut Canvas, offset: (usize, usize)) {
    let inner_w = frame.width.saturating_sub(2);
    let inner_h = frame.height.saturating_sub(2);
    for dy in 0..inner_h {
        for dx in 0..inner_w {
            let sx = offset.0 + dx;
            let sy = offset.1 + dy;
            if sx < canvas.width && sy < canvas.height {
                frame.put(dx + 1, dy + 1, canvas.cells[sy][sx]);
            }
        }
    }
}

fn frame_border(frame: &mut Canvas, title: &str, footer: &str) {
    if frame.width < 2 || frame.height < 2 {
        return;
    }
    frame.put(0, 0, '┌');
    frame.horizontal(1, frame.width - 2, 0, '─');
    frame.put(frame.width - 1, 0, '┐');
    frame.put(0, frame.height - 1, '└');
    frame.horizontal(1, frame.width - 2, frame.height - 1, '─');
    frame.put(frame.width - 1, frame.height - 1, '┘');
    for y in 1..frame.height - 1 {
        frame.put(0, y, '│');
        frame.put(frame.width - 1, y, '│');
    }
    frame.text(2, 0, title, frame.width.saturating_sub(4));
    let footer_len = footer.chars().count();
    let footer_x = frame.width.saturating_sub(footer_len + 2).max(1);
    frame.text(footer_x, frame.height - 1, footer, footer_len);
}

fn render_flow(
    projection: &LineageProjection,
    scene: &SceneGeometry,
    width: usize,
    height: usize,
) -> String {
    let mut canvas = Canvas::new(scene.canvas_bounds.w, scene.canvas_bounds.h);
    for segment in &scene.edge_segments {
        draw_segment(&mut canvas, segment);
    }
    let by_id: HashMap<&str, &Node> = projection.nodes.iter().map(|n| (n.id, n)).collect();
    for placed in &scene.node_rects {
        draw_box(
            &mut canvas,
            placed.rect,
            by_id.get(placed.id).expect("node for rect"),
        );
    }

    let viewport = Bounds {
        w: width.saturating_sub(2),
        h: height.saturating_sub(2),
    };
    let offset = camera(scene, viewport);
    let mut frame = Canvas::new(width, height);
    copy_viewport(&canvas, &mut frame, offset);
    let title = format!(
        " LINEAGE flow · node 5/7 · canvas {}x{} · offset +{}/+{} ",
        scene.canvas_bounds.w, scene.canvas_bounds.h, offset.0, offset.1
    );
    frame_border(
        &mut frame,
        &title,
        " legend ─▸ spawn ═▸ relay ·?▸ unknown · Esc back · q quit ",
    );
    frame.into_string()
}

fn compact_rows(projection: &LineageProjection) -> Result<Vec<String>, String> {
    let index = build_index(projection)?;
    let children = children_of(projection, &index)?;
    let roots: Vec<usize> = projection
        .nodes
        .iter()
        .enumerate()
        .filter_map(|(i, n)| n.parent.is_none().then_some(i))
        .collect();
    let mut rows = Vec::new();

    fn visit(
        i: usize,
        depth: usize,
        projection: &LineageProjection,
        children: &[Vec<usize>],
        rows: &mut Vec<String>,
        visited: &mut HashSet<usize>,
    ) -> Result<(), String> {
        if !visited.insert(i) {
            return Err("cycle while rendering compact tree".to_string());
        }
        let node = &projection.nodes[i];
        let indent = "  ".repeat(depth);
        let edge = node.edge.map(EdgeKind::compact).unwrap_or("");
        match node.kind {
            NodeKind::Gap => {
                rows.push("─ partial lineage ─".to_string());
                rows.push(format!("{indent}{}", node.name));
            }
            NodeKind::Tombstone => rows.push(format!("{indent}{edge}{}", node.name)),
            NodeKind::Manual => {
                rows.push("─ standalone (display-only) ─".to_string());
                rows.push(format!("{} · {}", node.name, node.meta));
                rows.push(format!("  {}", node.status));
            }
            NodeKind::Generation => {
                let marker = if node.selected { "▶" } else { "" };
                rows.push(format!(
                    "{indent}{edge}{marker}{} · {}",
                    node.name, node.meta
                ));
                rows.push(format!("{indent}    {}", node.status));
            }
        }
        for &child in &children[i] {
            visit(child, depth + 1, projection, children, rows, visited)?;
        }
        Ok(())
    }

    let mut visited = HashSet::new();
    for root in roots {
        visit(root, 0, projection, &children, &mut rows, &mut visited)?;
    }
    Ok(rows)
}

fn render_compact(projection: &LineageProjection, width: usize, height: usize) -> String {
    let rows = compact_rows(projection).expect("valid compact projection");
    let mut frame = Canvas::new(width, height);
    frame_border(
        &mut frame,
        " LINEAGE compact · node 5/7 ",
        " Esc back · q quit ",
    );
    let max_rows = height.saturating_sub(2);
    let max_width = width.saturating_sub(2);
    for (y, row) in rows.iter().take(max_rows).enumerate() {
        frame.text(1, y + 1, row, max_width);
    }
    frame.into_string()
}

fn cycle_fixture() -> LineageProjection {
    let mut cycle = fixture();
    cycle.nodes.truncate(2);
    cycle.nodes[0].parent = Some("g-relay-1");
    cycle.nodes[0].edge = Some(EdgeKind::Spawn);
    cycle
}

fn self_check() -> Result<SceneGeometry, String> {
    let projection = fixture();
    let scene = flow_layout(&projection)?;
    validate_geometry(&projection, &scene)?;
    let repeated = flow_layout(&projection)?;
    if scene != repeated {
        return Err("layout is not deterministic".to_string());
    }
    if flow_layout(&cycle_fixture()).is_ok() {
        return Err("cycle fixture was not rejected".to_string());
    }
    for width in [120, 81] {
        let viewport = Bounds {
            w: width - 2,
            h: if width == 120 { 38 } else { 22 },
        };
        let offset = camera(&scene, viewport);
        let selected = scene
            .node_rects
            .iter()
            .find(|node| node.selected)
            .expect("selected fixture node");
        if selected.rect.x < offset.0
            || selected.rect.right() >= offset.0 + viewport.w
            || selected.rect.y < offset.1
            || selected.rect.bottom() >= offset.1 + viewport.h
        {
            return Err(format!("selected node is outside {width}-column viewport"));
        }
    }
    Ok(scene)
}

fn frame(width: usize, height: usize) -> Result<String, String> {
    let projection = fixture();
    if width > 80 {
        let scene = self_check()?;
        Ok(render_flow(&projection, &scene, width, height))
    } else {
        compact_rows(&projection)?;
        Ok(render_compact(&projection, width, height))
    }
}

fn write_goldens(dir: &Path) -> Result<(), String> {
    let cases = [
        (120, 40, "golden-120x40.txt"),
        (81, 24, "golden-81x24.txt"),
        (80, 24, "golden-80x24.txt"),
    ];
    for (width, height, name) in cases {
        let content = frame(width, height)?;
        fs::write(dir.join(name), content)
            .map_err(|e| format!("write {}: {e}", dir.join(name).display()))?;
    }
    Ok(())
}

fn usage() -> &'static str {
    "usage: flow_layout [--check | --width <cols> --height <rows> | --write-goldens <dir>]"
}

fn parse_usize(raw: Option<String>, flag: &str) -> Result<usize, String> {
    raw.ok_or_else(|| format!("{flag} requires a value"))?
        .parse()
        .map_err(|_| format!("{flag} requires a positive integer"))
}

fn run() -> Result<String, String> {
    let mut args = env::args().skip(1);
    let Some(first) = args.next() else {
        return frame(120, 40);
    };
    match first.as_str() {
        "--check" => {
            if args.next().is_some() {
                return Err(usage().to_string());
            }
            let scene = self_check()?;
            let mut out = String::new();
            writeln!(
                &mut out,
                "ok: {} rects, {} segments, {} selectable, canvas {}x{}",
                scene.node_rects.len(),
                scene.edge_segments.len(),
                scene.selectable_order.len(),
                scene.canvas_bounds.w,
                scene.canvas_bounds.h
            )
            .unwrap();
            Ok(out)
        }
        "--write-goldens" => {
            let dir = args
                .next()
                .ok_or_else(|| "--write-goldens requires a directory".to_string())?;
            if args.next().is_some() {
                return Err(usage().to_string());
            }
            write_goldens(Path::new(&dir))?;
            Ok("goldens written\n".to_string())
        }
        "--width" => {
            let width = parse_usize(args.next(), "--width")?;
            let flag = args.next().ok_or_else(|| usage().to_string())?;
            if flag != "--height" {
                return Err(usage().to_string());
            }
            let height = parse_usize(args.next(), "--height")?;
            if args.next().is_some() || width < 20 || height < 8 {
                return Err(usage().to_string());
            }
            frame(width, height)
        }
        _ => Err(usage().to_string()),
    }
}

fn main() {
    match run() {
        Ok(output) => print!("{output}"),
        Err(error) => {
            eprintln!("flow_layout: {error}");
            std::process::exit(1);
        }
    }
}
