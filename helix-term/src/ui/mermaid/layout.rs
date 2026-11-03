//! Layered graph layout.
//!
//! Nodes are assigned to layers along the flow direction, edges spanning several layers get
//! a dummy vertex in each layer they cross, layers are ordered to reduce crossings and
//! vertices are positioned so edges run straight where possible. Edges between two layers are
//! routed through a channel: a vertical stub, an optional horizontal track shared by all
//! edges leaving the same port, a label row and the arrowhead.
//!
//! The layout is computed in abstract coordinates, *depth* along the flow and *breadth*
//! across it, and mapped to the canvas for the diagram's direction.

use std::collections::{HashMap, VecDeque};

use super::{width, Canvas, Line, NodeBox, Role, Stroke, DOWN, LEFT, RIGHT, UP};

const MAX_NODES: usize = 150;
const MAX_EDGES: usize = 300;
/// Nodes plus the dummy vertices of long edges.
const MAX_VERTICES: usize = 2000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Direction {
    Down,
    Up,
    Right,
    Left,
}

impl Direction {
    pub(super) fn parse(text: &str) -> Option<Direction> {
        match text.trim().to_ascii_uppercase().as_str() {
            "TD" | "TB" => Some(Direction::Down),
            "BT" => Some(Direction::Up),
            "LR" => Some(Direction::Right),
            "RL" => Some(Direction::Left),
            _ => None,
        }
    }

    fn vertical(self) -> bool {
        matches!(self, Direction::Down | Direction::Up)
    }

    /// The canvas direction of the flow.
    fn forward(self) -> u8 {
        match self {
            Direction::Down => DOWN,
            Direction::Up => UP,
            Direction::Right => RIGHT,
            Direction::Left => LEFT,
        }
    }

    fn backward(self) -> u8 {
        match self {
            Direction::Down => UP,
            Direction::Up => DOWN,
            Direction::Right => LEFT,
            Direction::Left => RIGHT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Head {
    None,
    Arrow,
    Circle,
    Cross,
    Triangle,
    Diamond,
    HollowDiamond,
}

impl Head {
    /// The glyph for a head pointing in canvas direction `toward`.
    fn glyph(self, toward: u8) -> Option<char> {
        let index = match toward {
            DOWN => 0,
            UP => 1,
            RIGHT => 2,
            _ => 3,
        };
        Some(match self {
            Head::None => return None,
            Head::Arrow => ['▼', '▲', '►', '◄'][index],
            Head::Triangle => ['▽', '△', '▷', '◁'][index],
            Head::Circle => '○',
            Head::Cross => '×',
            Head::Diamond => '◆',
            Head::HollowDiamond => '◇',
        })
    }
}

#[derive(Debug, Clone)]
pub(super) struct Edge {
    pub from: usize,
    pub to: usize,
    pub label: String,
    pub stroke: Stroke,
    /// The end at `to`.
    pub head: Head,
    /// The end at `from`.
    pub tail: Head,
    /// Minimum number of layers between the ends.
    pub minlen: usize,
}

impl Edge {
    pub(super) fn new(from: usize, to: usize) -> Self {
        Edge {
            from,
            to,
            label: String::new(),
            stroke: Stroke::Solid,
            head: Head::Arrow,
            tail: Head::None,
            minlen: 1,
        }
    }
}

pub(super) struct Graph {
    pub direction: Direction,
    pub nodes: Vec<NodeBox>,
    pub ids: HashMap<String, usize>,
    pub edges: Vec<Edge>,
}

impl Graph {
    pub(super) fn new(direction: Direction) -> Self {
        Graph {
            direction,
            nodes: Vec::new(),
            ids: HashMap::new(),
            edges: Vec::new(),
        }
    }

    /// The node with `id`, created with the id as its text if it does not exist yet.
    pub(super) fn node(&mut self, id: &str) -> usize {
        if let Some(&index) = self.ids.get(id) {
            return index;
        }
        let index = self.nodes.len();
        self.nodes
            .push(NodeBox::new(vec![id.to_string()], super::Shape::Rect));
        self.ids.insert(id.to_string(), index);
        index
    }
}

/// Lay out and draw a graph. Horizontal graphs that are too wide are drawn vertically, and
/// node text is wrapped narrower if that is still too wide.
pub(super) fn render(graph: &Graph, max_width: usize) -> Option<Vec<Line>> {
    if graph.nodes.is_empty() || graph.nodes.len() > MAX_NODES || graph.edges.len() > MAX_EDGES {
        return None;
    }
    let vertical = match graph.direction {
        Direction::Up => Direction::Up,
        _ => Direction::Down,
    };
    let mut attempts = vec![(graph.direction, 24)];
    if !graph.direction.vertical() {
        attempts.push((vertical, 24));
    }
    attempts.push((vertical, 12));
    let mut narrowest: Option<Canvas> = None;
    for (direction, wrap) in attempts {
        let canvas = layout(graph, direction, wrap)?;
        if canvas.width <= max_width {
            return Some(canvas.into_lines());
        }
        if narrowest
            .as_ref()
            .is_none_or(|narrowest| canvas.width < narrowest.width)
        {
            narrowest = Some(canvas);
        }
    }
    narrowest.map(Canvas::into_lines)
}

struct Vertex {
    layer: usize,
    /// Size across the flow.
    breadth: usize,
    /// Size along the flow.
    depth: usize,
    /// The node drawn here, `None` for dummy vertices of long edges.
    node: Option<usize>,
}

/// The part of an edge between two adjacent layers.
struct Segment {
    upper: usize,
    lower: usize,
    edge: usize,
    /// Whether `upper`, respectively `lower`, is an end of the edge rather than a dummy.
    first: bool,
    last: bool,
}

/// Edges leaving the same port, routed together.
struct Net {
    upper: usize,
    x: i64,
    /// (port on the lower vertex, segment)
    drops: Vec<(i64, usize)>,
    lo: i64,
    hi: i64,
    track: usize,
}

impl Net {
    fn bends(&self) -> bool {
        self.drops.iter().any(|&(x, _)| x != self.x)
    }
}

struct Gap {
    nets: Vec<Net>,
    tracks: usize,
    /// Depth of the label region, 0 without labels.
    label_depth: usize,
    /// Joined labels by (lower vertex, port).
    labels: Vec<(usize, i64, String)>,
    start: usize,
}

fn layout(graph: &Graph, direction: Direction, wrap: usize) -> Option<Canvas> {
    let vertical = direction.vertical();
    let boxes: Vec<NodeBox> = graph.nodes.iter().map(|node| node.wrapped(wrap)).collect();
    let node_count = boxes.len();
    let edges: Vec<&Edge> = graph.edges.iter().filter(|e| e.from != e.to).collect();

    // Layers.
    let reversed = break_cycles(node_count, &edges);
    let oriented: Vec<(usize, usize)> = edges
        .iter()
        .zip(&reversed)
        .map(|(edge, &rev)| {
            if rev {
                (edge.to, edge.from)
            } else {
                (edge.from, edge.to)
            }
        })
        .collect();
    let minlen: Vec<usize> = edges.iter().map(|edge| edge.minlen.max(1)).collect();
    let layer_of = assign_layers(node_count, &oriented, &minlen);

    // Vertices and segments.
    let mut vertices: Vec<Vertex> = boxes
        .iter()
        .enumerate()
        .map(|(index, node)| {
            let (w, h) = node.size();
            let (breadth, depth) = if vertical { (w, h) } else { (h, w) };
            Vertex {
                layer: layer_of[index],
                breadth,
                depth,
                node: Some(index),
            }
        })
        .collect();
    let mut segments = Vec::new();
    for (edge, &(a, b)) in oriented.iter().enumerate() {
        let mut previous = a;
        for layer in layer_of[a] + 1..layer_of[b] {
            let dummy = vertices.len();
            vertices.push(Vertex {
                layer,
                breadth: 1,
                depth: 0,
                node: None,
            });
            segments.push(Segment {
                upper: previous,
                lower: dummy,
                edge,
                first: previous == a,
                last: false,
            });
            previous = dummy;
        }
        segments.push(Segment {
            upper: previous,
            lower: b,
            edge,
            first: previous == a,
            last: true,
        });
    }
    if vertices.len() > MAX_VERTICES {
        return None;
    }
    let layer_count = vertices.iter().map(|v| v.layer + 1).max().unwrap_or(1);
    let mut layers: Vec<Vec<usize>> = vec![Vec::new(); layer_count];
    for (index, vertex) in vertices.iter().enumerate() {
        layers[vertex.layer].push(index);
    }
    let mut uppers = vec![Vec::new(); vertices.len()];
    let mut lowers = vec![Vec::new(); vertices.len()];
    // Segments below and above each vertex.
    let mut below = vec![Vec::new(); vertices.len()];
    let mut above = vec![Vec::new(); vertices.len()];
    for (index, segment) in segments.iter().enumerate() {
        lowers[segment.upper].push(segment.lower);
        uppers[segment.lower].push(segment.upper);
        below[segment.upper].push(index);
        above[segment.lower].push(index);
    }
    order_layers(&mut layers, &uppers, &lowers, vertices.len());

    // Heads at either end of a segment, in layout orientation.
    let upper_head = |s: &Segment| {
        let edge = edges[s.edge];
        match (s.first, reversed[s.edge]) {
            (false, _) => Head::None,
            (true, false) => edge.tail,
            (true, true) => edge.head,
        }
    };
    let lower_head = |s: &Segment| {
        let edge = edges[s.edge];
        match (s.last, reversed[s.edge]) {
            (false, _) => Head::None,
            (true, false) => edge.head,
            (true, true) => edge.tail,
        }
    };

    // Labels entering each vertex, used to keep room for them.
    let mut label_width = vec![0usize; vertices.len()];
    for segment in &segments {
        let label = &edges[segment.edge].label;
        if segment.last && !label.is_empty() {
            label_width[segment.lower] += width(label) + 2;
        }
    }

    // Positions across the flow.
    let size = |v: usize| vertices[v].breadth as i64;
    let spacing = |a: usize, b: usize| -> i64 {
        let real = vertices[a].node.is_some() && vertices[b].node.is_some();
        let mut gap = match (vertical, real) {
            (true, true) => 4,
            (true, false) => 2,
            (false, _) => 1,
        };
        if vertical && label_width[a] > 0 {
            // Labels are drawn right of the line entering `a`'s center.
            let need = label_width[a] as i64 + 2 - (size(a) - size(a) / 2) - size(b) / 2;
            gap = gap.max(need);
        }
        gap
    };
    let mut pos = vec![0i64; vertices.len()];
    for layer in &layers {
        let mut x = 0;
        for (i, &v) in layer.iter().enumerate() {
            pos[v] = x;
            if let Some(&next) = layer.get(i + 1) {
                x += size(v) + spacing(v, next);
            }
        }
    }
    for iteration in 0..9 {
        let down = iteration % 2 == 0;
        let order: Vec<usize> = if down {
            (1..layer_count).collect()
        } else {
            (0..layer_count.saturating_sub(1)).rev().collect()
        };
        for l in order {
            let desired: Vec<i64> = layers[l]
                .iter()
                .map(|&v| {
                    let neighbors = if down { &uppers[v] } else { &lowers[v] };
                    if neighbors.is_empty() {
                        return pos[v];
                    }
                    let sum: i64 = neighbors.iter().map(|&u| pos[u] + size(u) / 2).sum();
                    let center = (sum as f64 / neighbors.len() as f64).round() as i64;
                    center - size(v) / 2
                })
                .collect();
            let placed = place(&layers[l], &desired, size, spacing);
            for (&v, x) in layers[l].iter().zip(placed) {
                pos[v] = x;
            }
        }
    }
    let min = pos.iter().copied().min().unwrap_or(0);
    for x in &mut pos {
        *x -= min;
    }

    // Ports: edges of the main flow share the center of a side. Back edges and edges with a
    // head at their upper end share a port per kind of head, so for example all subclasses
    // meet at one inheritance triangle.
    let mut upper_port = vec![0i64; segments.len()];
    let mut lower_port = vec![0i64; segments.len()];
    let center = |v: usize| pos[v] + size(v) / 2;
    for v in 0..vertices.len() {
        let below: Vec<(usize, PortKey, i64)> = below[v]
            .iter()
            .map(|&s| {
                let key = (reversed[segments[s].edge], upper_head(&segments[s]));
                (s, key, center(segments[s].lower))
            })
            .collect();
        let above: Vec<(usize, PortKey, i64)> = above[v]
            .iter()
            .map(|&s| {
                let key = (reversed[segments[s].edge], Head::None);
                (s, key, center(segments[s].upper))
            })
            .collect();
        for (s, x) in ports(pos[v], size(v), &below) {
            upper_port[s] = x;
        }
        for (s, x) in ports(pos[v], size(v), &above) {
            lower_port[s] = x;
        }
    }

    // Channels between layers.
    let mut gaps: Vec<Gap> = Vec::new();
    for l in 0..layer_count.saturating_sub(1) {
        let mut nets: Vec<Net> = Vec::new();
        let mut labels: Vec<(usize, i64, String)> = Vec::new();
        for (s, segment) in segments.iter().enumerate() {
            if vertices[segment.upper].layer != l {
                continue;
            }
            let x = upper_port[s];
            let drop = lower_port[s];
            match nets
                .iter_mut()
                .find(|net| net.upper == segment.upper && net.x == x)
            {
                Some(net) => net.drops.push((drop, s)),
                None => nets.push(Net {
                    upper: segment.upper,
                    x,
                    drops: vec![(drop, s)],
                    lo: 0,
                    hi: 0,
                    track: 0,
                }),
            }
            let label = &edges[segment.edge].label;
            if segment.last && !label.is_empty() {
                match labels
                    .iter_mut()
                    .find(|(v, port, _)| *v == segment.lower && *port == drop)
                {
                    Some((_, _, text)) => {
                        if !text.split(", ").any(|existing| existing == label) {
                            text.push_str(", ");
                            text.push_str(label);
                        }
                    }
                    None => labels.push((segment.lower, drop, label.clone())),
                }
            }
        }
        for net in &mut nets {
            net.lo = net.drops.iter().map(|d| d.0).chain([net.x]).min().unwrap();
            net.hi = net.drops.iter().map(|d| d.0).chain([net.x]).max().unwrap();
        }
        let tracks = assign_tracks(&mut nets);
        let label_depth = match labels.iter().map(|(_, _, text)| width(text)).max() {
            None => 0,
            Some(_) if vertical => 1,
            Some(widest) => widest + 4,
        };
        gaps.push(Gap {
            nets,
            tracks,
            label_depth,
            labels,
            start: 0,
        });
    }

    // Depth coordinates.
    let layer_depth: Vec<usize> = layers
        .iter()
        .map(|layer| {
            layer
                .iter()
                .map(|&v| vertices[v].depth)
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();
    let mut layer_start = vec![0usize; layer_count];
    let mut depth = 0;
    for l in 0..layer_count {
        layer_start[l] = depth;
        depth += layer_depth[l];
        if let Some(gap) = gaps.get_mut(l) {
            gap.start = depth;
            depth += 2 + gap.tracks + gap.label_depth;
        }
    }
    let total_depth = depth;
    let mut total_breadth = (0..vertices.len())
        .map(|v| (pos[v] + size(v)) as usize)
        .max()
        .unwrap_or(1);
    if vertical {
        for gap in &gaps {
            for (_, port, text) in &gap.labels {
                total_breadth = total_breadth.max(*port as usize + 2 + width(text));
            }
        }
    }

    let map = |d: usize, b: i64| -> (usize, usize) {
        let b = b as usize;
        match direction {
            Direction::Down => (d, b),
            Direction::Up => (total_depth - 1 - d, b),
            Direction::Right => (b, d),
            Direction::Left => (b, total_depth - 1 - d),
        }
    };
    let (height, canvas_width) = if vertical {
        (total_depth, total_breadth)
    } else {
        (total_breadth, total_depth)
    };
    let mut canvas = Canvas::new(canvas_width.max(1), height.max(1));
    let forward = direction.forward();
    let backward = direction.backward();

    // Where a segment leaves its upper vertex and enters its lower one.
    let exit_depth = |v: usize| match vertices[v].node {
        Some(_) => layer_start[vertices[v].layer] + vertices[v].depth - 1,
        None => layer_start[vertices[v].layer],
    };

    // Lines.
    for (l, gap) in gaps.iter().enumerate() {
        let end = layer_start[l + 1];
        for net in &gap.nets {
            let start = exit_depth(net.upper);
            let track = gap.start + 1 + net.track;
            for &(drop, s) in &net.drops {
                let stroke = edges[segments[s].edge].stroke;
                if net.bends() {
                    canvas.segment(map(start, net.x), map(track, net.x), stroke);
                    canvas.segment(map(track, net.x), map(track, drop), stroke);
                    canvas.segment(map(track, drop), map(end, drop), stroke);
                } else {
                    canvas.segment(map(start, net.x), map(end, drop), stroke);
                }
            }
        }
    }

    // Nodes.
    for (v, vertex) in vertices.iter().enumerate() {
        let Some(node) = vertex.node else { continue };
        let d = layer_start[vertex.layer];
        let b = pos[v] as usize;
        let (row, col) = match direction {
            Direction::Down => (d, b),
            Direction::Up => (total_depth - d - vertex.depth, b),
            Direction::Right => (b, d),
            Direction::Left => (b, total_depth - d - vertex.depth),
        };
        boxes[node].draw(&mut canvas, row, col);
    }

    // Junctions and arrowheads.
    for (l, gap) in gaps.iter().enumerate() {
        let end = layer_start[l + 1];
        for net in &gap.nets {
            if vertices[net.upper].node.is_none() {
                continue;
            }
            let start = exit_depth(net.upper);
            let head = net
                .drops
                .iter()
                .map(|&(_, s)| upper_head(&segments[s]))
                .find(|&head| head != Head::None);
            match head.and_then(|head| head.glyph(backward)) {
                Some(glyph) => {
                    let (row, col) = map(start + 1, net.x);
                    canvas.put(row, col, glyph, Role::Arrow);
                }
                None => {
                    let (row, col) = map(start, net.x);
                    canvas.attach(row, col, forward);
                }
            }
        }
        let mut entries: Vec<(usize, i64, Head)> = Vec::new();
        for net in &gap.nets {
            for &(drop, s) in &net.drops {
                let segment = &segments[s];
                if vertices[segment.lower].node.is_none() {
                    continue;
                }
                let head = lower_head(segment);
                match entries
                    .iter_mut()
                    .find(|(v, x, _)| *v == segment.lower && *x == drop)
                {
                    Some(entry) if entry.2 == Head::None => entry.2 = head,
                    Some(_) => {}
                    None => entries.push((segment.lower, drop, head)),
                }
            }
        }
        for (_, x, head) in entries {
            match head.glyph(forward) {
                Some(glyph) => {
                    let (row, col) = map(end - 1, x);
                    canvas.put(row, col, glyph, Role::Arrow);
                }
                None => {
                    let (row, col) = map(end, x);
                    canvas.attach(row, col, backward);
                }
            }
        }
    }

    // Labels.
    for gap in &gaps {
        let label_start = gap.start + 1 + gap.tracks;
        for (_, port, text) in &gap.labels {
            if vertical {
                let (row, col) = map(label_start, *port);
                if canvas.is_blank(row, col + 1) {
                    canvas.text_in_blank(row, col + 2, text, Role::Label);
                }
            } else {
                // On the line entering the node, centered in the label region.
                let (row, a) = map(label_start, *port);
                let (_, b) = map(label_start + gap.label_depth - 1, *port);
                let (first, last) = (a.min(b), a.max(b));
                let room = last + 1 - first;
                let text_width = width(text).min(room.saturating_sub(2));
                let col = first + (room - text_width) / 2;
                let free = (col.saturating_sub(1)..=col + text_width)
                    .all(|c| canvas.is_blank(row, c) || canvas.mask(row, c) == LEFT | RIGHT);
                if free {
                    canvas.put(row, col - 1, ' ', Role::Text);
                    canvas.put(row, col + text_width, ' ', Role::Text);
                    let clipped = super::clip(vec![(text.clone(), ())], text_width);
                    canvas.text(row, col, &clipped[0].0, Role::Label);
                }
            }
        }
    }

    Some(canvas)
}

/// Mark edges that close cycles (found by depth first search in declaration order) so they
/// can be laid out reversed.
fn break_cycles(node_count: usize, edges: &[&Edge]) -> Vec<bool> {
    let mut out = vec![Vec::new(); node_count];
    for (index, edge) in edges.iter().enumerate() {
        out[edge.from].push(index);
    }
    // 0: not visited, 1: on the stack, 2: done
    let mut state = vec![0u8; node_count];
    let mut reversed = vec![false; edges.len()];
    for root in 0..node_count {
        if state[root] != 0 {
            continue;
        }
        state[root] = 1;
        let mut stack = vec![(root, 0usize)];
        while let Some(top) = stack.last_mut() {
            let (v, next) = *top;
            if let Some(&e) = out[v].get(next) {
                top.1 += 1;
                let w = edges[e].to;
                match state[w] {
                    0 => {
                        state[w] = 1;
                        stack.push((w, 0));
                    }
                    1 => reversed[e] = true,
                    _ => {}
                }
            } else {
                state[v] = 2;
                stack.pop();
            }
        }
    }
    reversed
}

/// Longest path layering, then sources are moved down next to their successors.
fn assign_layers(node_count: usize, oriented: &[(usize, usize)], minlen: &[usize]) -> Vec<usize> {
    let mut indegree = vec![0usize; node_count];
    let mut out = vec![Vec::new(); node_count];
    for (index, &(a, b)) in oriented.iter().enumerate() {
        indegree[b] += 1;
        out[a].push(index);
    }
    let sources: Vec<bool> = indegree.iter().map(|&d| d == 0).collect();
    let mut layer = vec![0usize; node_count];
    let mut order = Vec::with_capacity(node_count);
    let mut queue: VecDeque<usize> = (0..node_count).filter(|&v| indegree[v] == 0).collect();
    while let Some(v) = queue.pop_front() {
        order.push(v);
        for &e in &out[v] {
            let b = oriented[e].1;
            layer[b] = layer[b].max(layer[v] + minlen[e]);
            indegree[b] -= 1;
            if indegree[b] == 0 {
                queue.push_back(b);
            }
        }
    }
    for &v in order.iter().rev() {
        if sources[v] {
            if let Some(below) = out[v]
                .iter()
                .map(|&e| layer[oriented[e].1] - minlen[e])
                .min()
            {
                layer[v] = below;
            }
        }
    }
    layer
}

/// Order vertices within layers by the barycenter heuristic, keeping the order with the
/// fewest crossings. The declaration order wins ties.
fn order_layers(
    layers: &mut [Vec<usize>],
    uppers: &[Vec<usize>],
    lowers: &[Vec<usize>],
    vertex_count: usize,
) {
    let mut index = vec![0usize; vertex_count];
    let reindex = |layers: &[Vec<usize>], index: &mut [usize]| {
        for layer in layers {
            for (i, &v) in layer.iter().enumerate() {
                index[v] = i;
            }
        }
    };
    reindex(layers, &mut index);
    let mut best = layers.to_vec();
    let mut best_crossings = crossings(layers, lowers, &index);
    for iteration in 0..16 {
        if best_crossings == 0 {
            break;
        }
        if iteration % 2 == 0 {
            for layer in layers.iter_mut().skip(1) {
                sort_by_barycenter(layer, uppers, &mut index);
            }
        } else {
            for layer in layers.iter_mut().rev().skip(1) {
                sort_by_barycenter(layer, lowers, &mut index);
            }
        }
        let count = crossings(layers, lowers, &index);
        if count < best_crossings {
            best = layers.to_vec();
            best_crossings = count;
        }
    }
    layers.clone_from_slice(&best);
}

fn sort_by_barycenter(layer: &mut Vec<usize>, neighbors: &[Vec<usize>], index: &mut [usize]) {
    let mut keyed: Vec<(f64, usize)> = layer
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let around = &neighbors[v];
            let key = if around.is_empty() {
                i as f64
            } else {
                around.iter().map(|&u| index[u] as f64).sum::<f64>() / around.len() as f64
            };
            (key, v)
        })
        .collect();
    keyed.sort_by(|a, b| a.0.total_cmp(&b.0));
    *layer = keyed.into_iter().map(|(_, v)| v).collect();
    for (i, &v) in layer.iter().enumerate() {
        index[v] = i;
    }
}

fn crossings(layers: &[Vec<usize>], lowers: &[Vec<usize>], index: &[usize]) -> usize {
    let mut count = 0;
    for layer in layers {
        let pairs: Vec<(usize, usize)> = layer
            .iter()
            .flat_map(|&v| lowers[v].iter().map(move |&w| (index[v], index[w])))
            .collect();
        for (i, a) in pairs.iter().enumerate() {
            for b in &pairs[i + 1..] {
                if (a.0 < b.0 && a.1 > b.1) || (a.0 > b.0 && a.1 < b.1) {
                    count += 1;
                }
            }
        }
    }
    count
}

/// Place a layer's vertices as close to their desired positions as their order and spacing
/// allow: the average of packing from the left and from the right.
fn place(
    layer: &[usize],
    desired: &[i64],
    size: impl Fn(usize) -> i64,
    spacing: impl Fn(usize, usize) -> i64,
) -> Vec<i64> {
    let count = layer.len();
    let mut left = vec![0i64; count];
    let mut right = vec![0i64; count];
    for i in 0..count {
        left[i] = if i == 0 {
            desired[0]
        } else {
            desired[i].max(left[i - 1] + size(layer[i - 1]) + spacing(layer[i - 1], layer[i]))
        };
    }
    for i in (0..count).rev() {
        right[i] = if i + 1 == count {
            desired[i]
        } else {
            desired[i].min(right[i + 1] - spacing(layer[i], layer[i + 1]) - size(layer[i]))
        };
    }
    (0..count)
        .map(|i| (left[i] + right[i]).div_euclid(2))
        .collect()
}

/// Segments sharing a port: (laid out reversed, head at the port).
type PortKey = (bool, Head);

/// Ports on one side of a vertex for `(segment, key, other end's center)`. Segments with the
/// same key share a port; the main flow, `(false, Head::None)`, gets the center.
fn ports(pos: i64, size: i64, segments: &[(usize, PortKey, i64)]) -> Vec<(usize, i64)> {
    const MAIN: PortKey = (false, Head::None);
    let center = pos + size / 2;
    let (lo, hi) = if size >= 3 {
        (pos + 1, pos + size - 2)
    } else {
        (pos, pos + size - 1)
    };
    // (key, segments, average center of the other ends)
    let mut groups: Vec<(PortKey, Vec<usize>, i64)> = Vec::new();
    for &(segment, key, other) in segments {
        match groups.iter_mut().find(|group| group.0 == key) {
            Some(group) => {
                group.1.push(segment);
                group.2 += other;
            }
            None => groups.push((key, vec![segment], other)),
        }
    }
    for group in &mut groups {
        group.2 /= group.1.len() as i64;
    }
    let has_main = groups.iter().any(|group| group.0 == MAIN);
    let (mut left, mut right): (Vec<_>, Vec<_>) = groups
        .iter()
        .filter(|group| group.0 != MAIN)
        .partition(|group| group.2 < center);
    // Nearest to the center first, so ports do not cross.
    left.sort_by_key(|group| std::cmp::Reverse(group.2));
    right.sort_by_key(|group| group.2);
    let (mut left_next, mut right_next) = match (has_main, left.is_empty(), right.is_empty()) {
        (true, _, _) => (center - 2, center + 2),
        (false, true, _) | (false, _, true) => (center, center),
        (false, false, false) => (center - 1, center + 1),
    };
    let mut result = Vec::new();
    for group in &groups {
        if group.0 == MAIN {
            result.extend(group.1.iter().map(|&s| (s, center)));
        }
    }
    for group in left {
        result.extend(group.1.iter().map(|&s| (s, left_next.clamp(lo, hi))));
        left_next -= 2;
    }
    for group in right {
        result.extend(group.1.iter().map(|&s| (s, right_next.clamp(lo, hi))));
        right_next += 2;
    }
    result
}

/// Assign tracks to bending nets, sharing a track between nets that do not overlap. A net
/// leaving at a column another net drops into must run above it, or their vertical lines
/// would merge. Returns the number of tracks.
fn assign_tracks(nets: &mut [Net]) -> usize {
    let bending: Vec<usize> = (0..nets.len()).filter(|&i| nets[i].bends()).collect();
    let must_precede = |nets: &[Net], a: usize, b: usize| {
        a != b
            && nets[b]
                .drops
                .iter()
                .any(|&(x, _)| x == nets[a].x && x != nets[b].x)
    };
    let mut done = vec![false; nets.len()];
    let mut remaining = bending.len();
    let mut track = 0;
    while remaining > 0 {
        let mut candidates: Vec<usize> = bending
            .iter()
            .copied()
            .filter(|&i| !done[i])
            .filter(|&i| {
                bending
                    .iter()
                    .all(|&j| done[j] || !must_precede(nets, j, i))
            })
            .collect();
        if candidates.is_empty() {
            // A cycle of constraints: place the leftmost remaining net alone.
            candidates = bending.iter().copied().filter(|&i| !done[i]).collect();
            candidates.sort_by_key(|&i| nets[i].lo);
            candidates.truncate(1);
        }
        candidates.sort_by_key(|&i| nets[i].lo);
        let mut placed = Vec::new();
        let mut last_hi = i64::MIN;
        for i in candidates {
            if last_hi == i64::MIN || nets[i].lo > last_hi + 1 {
                nets[i].track = track;
                last_hi = nets[i].hi;
                placed.push(i);
            }
        }
        for i in placed {
            done[i] = true;
            remaining -= 1;
        }
        track += 1;
    }
    track
}

#[cfg(test)]
mod tests {
    use super::super::tests::plain;

    #[test]
    fn draws_a_chain() {
        let text = plain("graph TD\nA[Start] --> B[End]", 80);
        assert_eq!(
            text,
            "┌───────┐\n│ Start │\n└───┬───┘\n    │\n    ▼\n ┌─────┐\n │ End │\n └─────┘"
        );
    }

    #[test]
    fn draws_a_fan_out_with_labels() {
        let text = plain("flowchart TD\nA{ok?} -->|yes| B\nA -->|no| C", 80);
        assert!(text.contains("╱─────╲"), "{text}");
        assert!(text.contains("yes"), "{text}");
        assert!(text.contains("no"), "{text}");
        assert!(text.contains('┴'), "{text}");
        assert_eq!(text.matches('▼').count(), 2, "{text}");
    }

    #[test]
    fn draws_left_to_right() {
        let text = plain("graph LR\nA --> B -- go --> C", 80);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert_eq!(lines[1], "│ A ├─►│ B ├── go ─►│ C │", "{text}");
    }

    #[test]
    fn wide_horizontal_graphs_turn_vertical() {
        let source =
            "graph LR\nA[a long node name] --> B[another long name] --> C[a third long name]";
        let text = plain(source, 40);
        assert!(
            text.lines().all(|line| line.chars().count() <= 40),
            "{text}"
        );
        assert!(text.contains('▼'), "{text}");
    }

    #[test]
    fn handles_cycles_and_long_edges() {
        let text = plain("graph TD\nA --> B --> C --> A\nA --> C", 80);
        assert!(text.contains('▲'), "{text}");
        for name in ["A", "B", "C"] {
            assert!(text.contains(&format!("│ {name} │")), "{text}");
        }
    }

    #[test]
    fn draws_bottom_to_top() {
        let text = plain("graph BT\nA --> B", 80);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[1].contains('B'), "{text}");
        assert!(lines[lines.len() - 2].contains('A'), "{text}");
        assert!(text.contains('▲'), "{text}");
    }
}
