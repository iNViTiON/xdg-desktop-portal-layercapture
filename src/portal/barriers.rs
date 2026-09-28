//! Zones, pointer barriers and release positions: pure logic, no I/O.
//!
//! Coordinates are global logical pixels (xdg-output), the space GetZones reports and clients
//! use for barriers and cursor positions.
//!
//! Barrier convention (portal spec and KDE Connect): a left edge is `x = X`, a right edge is
//! `x = X + W` ("one past"), top `y = Y`, bottom `y = Y + H`. The span along the edge may end at
//! `Y + H - 1` (KDE Connect) or `Y + H` (mutter); both are accepted. Barriers must lie on an
//! outer edge: an edge shared with a neighbouring output cannot be a barrier.

use std::collections::HashMap;

use zbus::zvariant::{OwnedValue, Value};

use crate::wayland::Edge;
use crate::wayland::outputs::Rect;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Zone {
    /// Output global name (0 for fake zones).
    pub output: u32,
    pub rect: Rect,
}

/// A validated barrier, mapped to a strip on one output edge. `start..end` is output-local
/// along the edge, already shrunk by the corner margin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarrierSpec {
    pub id: u32,
    pub output: u32,
    pub edge: Edge,
    pub start: i32,
    pub end: i32,
}

/// Reads `barrier_id` (`u` per spec, `i` from KDE Connect) and `position` (`(iiii)` per spec,
/// `ai` from KDE Connect). Returns the id if it could be read, so it can be reported failed.
pub fn parse_barrier(dict: &HashMap<String, OwnedValue>) -> Result<(u32, [i32; 4]), Option<u32>> {
    let id = match dict.get("barrier_id").map(|v| &**v) {
        Some(Value::U32(id)) => Some(*id),
        Some(Value::I32(id)) if *id > 0 => Some(*id as u32),
        _ => None,
    };
    let pos = dict.get("position").and_then(|v| position(v));
    match (id, pos) {
        (Some(id), Some(pos)) if id != 0 => Ok((id, pos)),
        (id, _) => Err(id),
    }
}

fn position(v: &Value<'_>) -> Option<[i32; 4]> {
    let ints: Vec<i32> = match v {
        Value::Structure(s) => s
            .fields()
            .iter()
            .map(|f| match f {
                Value::I32(i) => Some(*i),
                _ => None,
            })
            .collect::<Option<_>>()?,
        Value::Array(a) => a
            .iter()
            .map(|f| match f {
                Value::I32(i) => Some(*i),
                _ => None,
            })
            .collect::<Option<_>>()?,
        Value::Value(inner) => return position(inner),
        _ => return None,
    };
    ints.try_into().ok()
}

fn overlaps(a0: i32, a1: i32, b0: i32, b1: i32) -> bool {
    a0 < b1 && b0 < a1
}

/// Validates a barrier against the zones and maps it to a strip, or `None` if it must be
/// reported as failed.
pub fn validate(id: u32, [x1, y1, x2, y2]: [i32; 4], zones: &[Zone], corner_margin: i32) -> Option<BarrierSpec> {
    if id == 0 {
        return None;
    }
    let vertical = x1 == x2 && y1 != y2;
    let horizontal = y1 == y2 && x1 != x2;
    if !vertical && !horizontal {
        return None;
    }
    let (fixed, lo, hi) = if vertical { (x1, y1.min(y2), y1.max(y2)) } else { (y1, x1.min(x2), x1.max(x2)) };

    for z in zones {
        let r = z.rect;
        // (edge, along-start, along-len) for this orientation.
        let (a0, len) = if vertical { (r.y, r.height) } else { (r.x, r.width) };
        let edge = if vertical {
            if fixed == r.x {
                Edge::Left
            } else if fixed == r.x + r.width {
                Edge::Right
            } else {
                continue;
            }
        } else if fixed == r.y {
            Edge::Top
        } else if fixed == r.y + r.height {
            Edge::Bottom
        } else {
            continue;
        };
        if lo < a0 || hi > a0 + len {
            continue;
        }
        // Reject edges shared with another zone over this span.
        let shared = zones.iter().any(|o| {
            if o == z {
                return false;
            }
            let q = o.rect;
            match edge {
                Edge::Left => q.x + q.width == r.x && overlaps(q.y, q.y + q.height, lo, hi + 1),
                Edge::Right => q.x == r.x + r.width && overlaps(q.y, q.y + q.height, lo, hi + 1),
                Edge::Top => q.y + q.height == r.y && overlaps(q.x, q.x + q.width, lo, hi + 1),
                Edge::Bottom => q.y == r.y + r.height && overlaps(q.x, q.x + q.width, lo, hi + 1),
            }
        });
        if shared {
            return None;
        }
        let start = (lo - a0).max(corner_margin);
        let end = ((hi + 1).min(a0 + len) - a0).min(len - corner_margin);
        if end - start < 1 {
            return None;
        }
        return Some(BarrierSpec { id, output: z.output, edge, start, end });
    }
    None
}

/// Global position of a strip's surface origin.
pub fn strip_origin(spec: &BarrierSpec, rect: Rect) -> (f64, f64) {
    match spec.edge {
        Edge::Left => (rect.x as f64, (rect.y + spec.start) as f64),
        Edge::Right => ((rect.x + rect.width - 1) as f64, (rect.y + spec.start) as f64),
        Edge::Top => ((rect.x + spec.start) as f64, rect.y as f64),
        Edge::Bottom => ((rect.x + spec.start) as f64, (rect.y + rect.height - 1) as f64),
    }
}

/// Where the pointer goes when a capture ends: `requested` (a client's Release position,
/// possibly on the barrier line, outside the zone, or in phone pixels) or `fallback` (the
/// entry point), clamped into the strip's span, and at least 2 px away from the 1 px strip so
/// it does not re-enter it at once.
pub fn release_target(spec: &BarrierSpec, rect: Rect, requested: Option<(f64, f64)>, fallback: (f64, f64)) -> (f64, f64) {
    let (x, y) = requested.filter(|(x, y)| x.is_finite() && y.is_finite()).unwrap_or(fallback);
    let clamp_along = |v: f64, a0: i32| v.clamp((a0 + spec.start) as f64, (a0 + spec.end - 1) as f64);
    match spec.edge {
        Edge::Left => ((rect.x + 2) as f64, clamp_along(y, rect.y)),
        Edge::Right => ((rect.x + rect.width - 3) as f64, clamp_along(y, rect.y)),
        Edge::Top => (clamp_along(x, rect.x), (rect.y + 2) as f64),
        Edge::Bottom => (clamp_along(x, rect.x), (rect.y + rect.height - 3) as f64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{Array, Structure};

    fn z(output: u32, x: i32, y: i32, width: i32, height: i32) -> Zone {
        Zone { output, rect: Rect { x, y, width, height } }
    }

    const ONE: [Zone; 1] = [Zone { output: 7, rect: Rect { x: 0, y: 0, width: 2880, height: 1800 } }];

    #[test]
    fn kde_connect_left_edge() {
        // KDE Connect: (x, y, x, y+h-1) with the default left edge.
        let s = validate(1, [0, 0, 0, 1799], &ONE, 8).unwrap();
        assert_eq!(s, BarrierSpec { id: 1, output: 7, edge: Edge::Left, start: 8, end: 1792 });
    }

    #[test]
    fn right_edge_is_one_past_and_bottom_may_be_inclusive_or_one_past() {
        let r = validate(1, [2880, 0, 2880, 1799], &ONE, 8).unwrap();
        assert_eq!(r.edge, Edge::Right);
        assert!(validate(1, [2879, 0, 2879, 1799], &ONE, 8).is_none());
        let b = validate(2, [0, 1800, 2880, 1800], &ONE, 0).unwrap();
        assert_eq!((b.edge, b.start, b.end), (Edge::Bottom, 0, 2880));
        let b = validate(2, [0, 1800, 2879, 1800], &ONE, 0).unwrap();
        assert_eq!(b.end, 2880);
    }

    #[test]
    fn rejects_bad_barriers() {
        assert!(validate(0, [0, 0, 0, 1799], &ONE, 8).is_none(), "id 0");
        assert!(validate(1, [0, 0, 10, 10], &ONE, 8).is_none(), "diagonal");
        assert!(validate(1, [0, 5, 0, 5], &ONE, 8).is_none(), "point");
        assert!(validate(1, [100, 0, 100, 1799], &ONE, 8).is_none(), "not on an edge");
        assert!(validate(1, [0, 0, 0, 5000], &ONE, 8).is_none(), "longer than the zone");
        assert!(validate(1, [0, 0, 0, 4], &ONE, 8).is_none(), "entirely inside the corner margin");
    }

    #[test]
    fn shared_edges_are_rejected_outer_edges_accepted() {
        let two = [z(1, 0, 0, 1920, 1080), z(2, 1920, 0, 1920, 1080)];
        assert!(validate(1, [1920, 0, 1920, 1079], &two, 0).is_none(), "shared middle edge");
        assert_eq!(validate(1, [0, 0, 0, 1079], &two, 0).unwrap().output, 1);
        assert_eq!(validate(1, [3840, 0, 3840, 1079], &two, 0).unwrap().output, 2);
    }

    #[test]
    fn parses_both_encodings() {
        let mut spec = HashMap::new();
        spec.insert("barrier_id".to_owned(), OwnedValue::from(3u32));
        spec.insert(
            "position".to_owned(),
            OwnedValue::try_from(Value::from(Structure::from((0i32, 0i32, 0i32, 1799i32)))).unwrap(),
        );
        assert_eq!(parse_barrier(&spec), Ok((3, [0, 0, 0, 1799])));

        let mut kc = HashMap::new();
        kc.insert("barrier_id".to_owned(), OwnedValue::from(1i32));
        kc.insert("position".to_owned(), OwnedValue::try_from(Value::from(Array::from(vec![0i32, 0, 0, 1799]))).unwrap());
        assert_eq!(parse_barrier(&kc), Ok((1, [0, 0, 0, 1799])));

        let mut bad = HashMap::new();
        bad.insert("barrier_id".to_owned(), OwnedValue::from(4u32));
        bad.insert("position".to_owned(), OwnedValue::try_from(Value::from(Array::from(vec![0i32, 0, 0]))).unwrap());
        assert_eq!(parse_barrier(&bad), Err(Some(4)));
    }

    #[test]
    fn release_target_clamps_and_insets() {
        let rect = ONE[0].rect;
        let left = validate(1, [0, 0, 0, 1799], &ONE, 8).unwrap();
        // KDE Connect sends (0, phoneY) with phone pixels on y.
        assert_eq!(release_target(&left, rect, Some((0.0, 2400.0)), (0.0, 500.0)), (2.0, 1791.0));
        assert_eq!(release_target(&left, rect, None, (0.0, 500.0)), (2.0, 500.0));
        assert_eq!(release_target(&left, rect, Some((f64::NAN, 1.0)), (0.0, 500.0)), (2.0, 500.0));
        let right = validate(1, [2880, 0, 2880, 1799], &ONE, 8).unwrap();
        assert_eq!(release_target(&right, rect, Some((2880.0, 3.0)), (0.0, 0.0)), (2877.0, 8.0));
        assert_eq!(strip_origin(&right, rect), (2879.0, 8.0));
    }
}
