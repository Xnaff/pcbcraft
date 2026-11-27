//! Reroute selected schematic wires between their unchanged endpoints.
//!
//! Cleanup only replaces the bends of selected wires. Endpoints, junctions, wire identities
//! and every unselected wire stay as they are, so the netlist cannot change. The editor and
//! MCP share [`Project::wire_cleanup`], which is deterministic for a given project.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::{Value, json};

use super::{bounds, heal, interior, path_contains_point, valid_point};
use crate::{
    CommandError, Project, Session,
    net_labels::{Label, Style},
    schematic::{GRID_NM, Point, Wire},
    selection::Selection,
};

/// Report what cleaning up the selected wires would change, without editing.
pub const PREVIEW: &str = "schematic.preview_wire_cleanup";
/// Reroute the selected wires exactly as [`PREVIEW`] reports, as one undo step.
pub const CLEAN_UP: &str = "schematic.clean_up_wires";

/// Most wires one cleanup may reroute; a larger selection is refused instead of truncated.
pub const MAX_WIRES: usize = 256;
/// Grid channels tried beyond the box around a wire's current route.
const CHANNEL_MARGIN: i64 = 8;
/// Most channels tried per axis; a longer wire keeps the channels nearest its middle.
const MAX_CHANNELS: i64 = 256;
/// What one crossing costs, in grid steps of extra length. A crossing is worth removing
/// with a two-bend detour only when that detour stays short.
const CROSSING_COST: i64 = 30;
/// What one bend costs, in grid steps of extra length.
const BEND_COST: i64 = 10;
/// Most candidate-to-neighbor route comparisons in one cleanup, so a dense selection is
/// refused instead of stalling the caller.
const WORK_BUDGET: usize = 16_000_000;
/// Rounds over the selection. Later rounds let a wire use room an earlier reroute freed.
const PASSES: usize = 3;

/// Bend, crossing and length totals of the selected wires.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Totals {
    /// Direction changes along the selected routes.
    pub bends: usize,
    /// Places where a selected wire crosses another wire without connecting to it.
    pub crossings: usize,
    pub length_nm: i64,
}

/// One wire whose route cleanup replaces, with both drawings for a before/after preview.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Reroute {
    #[serde(rename = "id", serialize_with = "wire_id")]
    pub wire: u64,
    pub before: Vec<Point>,
    pub after: Vec<Point>,
}

/// Publish the same `wire-N` identity that inspection and selections use.
fn wire_id<S: serde::Serializer>(ordinal: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format!("wire-{ordinal}"))
}

/// The reviewed result of one cleanup: only rerouted wires are listed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Cleanup {
    pub before: Totals,
    pub after: Totals,
    pub wires: Vec<Reroute>,
}

/// An axis-aligned box a route must not enter; touching its edge is allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Obstacle {
    low: Point,
    high: Point,
}

impl Obstacle {
    /// Build from any two opposite corners.
    fn new(a: Point, b: Point) -> Self {
        Self { low: Point { x_nm: a.x_nm.min(b.x_nm), y_nm: a.y_nm.min(b.y_nm) }, high: Point { x_nm: a.x_nm.max(b.x_nm), y_nm: a.y_nm.max(b.y_nm) } }
    }

    /// Whether the point lies strictly inside the box.
    fn contains(&self, at: Point) -> bool {
        self.low.x_nm < at.x_nm && at.x_nm < self.high.x_nm && self.low.y_nm < at.y_nm && at.y_nm < self.high.y_nm
    }

    /// Whether an orthogonal leg passes through the inside of the box.
    fn blocks(&self, a: Point, b: Point) -> bool {
        let (low, high) =
            (Point { x_nm: a.x_nm.min(b.x_nm), y_nm: a.y_nm.min(b.y_nm) }, Point { x_nm: a.x_nm.max(b.x_nm), y_nm: a.y_nm.max(b.y_nm) });
        if a.y_nm == b.y_nm {
            self.low.y_nm < a.y_nm && a.y_nm < self.high.y_nm && low.x_nm < self.high.x_nm && high.x_nm > self.low.x_nm
        } else {
            self.low.x_nm < a.x_nm && a.x_nm < self.high.x_nm && low.y_nm < self.high.y_nm && high.y_nm > self.low.y_nm
        }
    }
}

/// The box a named-net label's glyph and name occupy, following the canvas layout: a local
/// name sits one grid above its anchor and always reads horizontally; a global flag extends
/// from its anchor along the label's rotation.
fn label_box(label: &Label) -> Obstacle {
    // shortcut: the name width is estimated at 0.6 of the text height per character and an
    // authored text offset is ignored; use real metrics once the engine owns text layout.
    let height = label.text.as_ref().map_or(GRID_NM, |text| text.height_nm);
    let width = label.name.chars().count() as i64 * height * 6 / 10;
    let turn = |x: i64, y: i64| match label.rotation {
        90 => (y, -x),
        180 => (-x, -y),
        270 => (-y, x),
        _ => (x, y),
    };
    let at = |(x, y): (i64, i64)| Point { x_nm: label.at.x_nm.saturating_add(x), y_nm: label.at.y_nm.saturating_add(y) };
    match label.style {
        Style::Local => {
            let (x, y) = turn(GRID_NM * 4 / 10, -GRID_NM);
            let reach = if label.rotation == 180 { -width } else { width };
            Obstacle::new(at((x, y - height / 2)), at((x + reach, y + height / 2)))
        }
        Style::Global => Obstacle::new(at(turn(0, -GRID_NM * 9 / 10)), at(turn(width + GRID_NM * 14 / 10, GRID_NM * 9 / 10))),
    }
}

/// Count direction changes, including a doubled-back leg.
fn bends(path: &[Point]) -> usize {
    path.windows(3)
        .filter(|run| (run[0].x_nm == run[1].x_nm) != (run[1].x_nm == run[2].x_nm) || heal::overlap(&run[..2], &run[1..]).is_some())
        .count()
}

/// Sum the leg lengths of an orthogonal route.
fn length(path: &[Point]) -> i64 {
    path.windows(2).map(|leg| (leg[0].x_nm - leg[1].x_nm).abs() + (leg[0].y_nm - leg[1].y_nm).abs()).sum()
}

/// Count points where a leg of one route crosses the inside of a leg of the other. Shared
/// ends, corners touching a leg and collinear overlaps are contacts, not crossings.
fn crossings(a: &[Point], b: &[Point]) -> usize {
    let inside = |low: i64, at: i64, high: i64| low.min(high) < at && at < low.max(high);
    a.windows(2)
        .flat_map(|leg| b.windows(2).map(move |other| (leg, other)))
        .filter(|(leg, other)| {
            let (flat, upright) = if leg[0].y_nm == leg[1].y_nm { (leg, other) } else { (other, leg) };
            flat[0].y_nm == flat[1].y_nm
                && upright[0].x_nm == upright[1].x_nm
                && inside(flat[0].x_nm, upright[0].x_nm, flat[1].x_nm)
                && inside(upright[0].y_nm, flat[0].y_nm, upright[1].y_nm)
        })
        .count()
}

/// What a candidate route is compared on, best first: clear of obstacles and other wires'
/// contacts, then the lowest weighted cost of its crossings, bends and length. Equal costs
/// prefer fewer crossings, then fewer bends.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Score {
    blocked: bool,
    cost_nm: i64,
    crossings: usize,
    bends: usize,
}

impl Score {
    /// Weigh crossings and bends as extra length; saturate instead of overflowing.
    fn new(blocked: bool, crossings: usize, bends: usize, length_nm: i64) -> Self {
        let weight = |count: usize, cost: i64| i64::try_from(count).unwrap_or(i64::MAX).saturating_mul(cost).saturating_mul(GRID_NM);
        Self {
            blocked,
            cost_nm: length_nm.saturating_add(weight(crossings, CROSSING_COST)).saturating_add(weight(bends, BEND_COST)),
            crossings,
            bends,
        }
    }
}

/// The fixed surroundings every candidate route of one cleanup is judged against.
struct Scene {
    obstacles: Vec<Obstacle>,
    /// Pins and hierarchy anchors: a route may end on one but never pass over it.
    contacts: Vec<Point>,
}

impl Scene {
    /// Collect symbols, sheet symbols, named-net labels and connectable points once. A
    /// symbol's box spans its body and pin leads, so its pins sit on the edge of the box.
    fn new(project: &Project) -> Self {
        let symbols = project.symbols.iter().filter_map(|symbol| {
            let body = crate::schematic_pdf::body_bounds(symbol, &symbol.unit_definition().drawing);
            bounds(body.into_iter().chain(symbol.pins())).map(|(low, high)| Obstacle::new(low, high))
        });
        let sheets = project.subsheets.iter().map(|block| {
            let size = project.sheet_symbol_size(block);
            Obstacle::new(block.at, Point { x_nm: block.at.x_nm.saturating_add(size.x_nm), y_nm: block.at.y_nm.saturating_add(size.y_nm) })
        });
        let obstacles = symbols.chain(sheets).chain(project.net_labels.iter().map(label_box)).collect();
        let contacts = project.symbols.iter().flat_map(|symbol| symbol.pins()).chain(project.hierarchy_endpoints().map(|(_, at)| at)).collect();
        Self { obstacles, contacts }
    }

    /// Score one route of a wire between `a` and `b` against every other wire's route.
    /// A route is blocked when it enters an obstacle that does not hold one of its own ends,
    /// passes over a foreign pin, touches another route's end or corner, puts a bend on
    /// another route, or runs along one: healing would later join such contacts.
    fn score(&self, path: &[Point], others: &[&[Point]]) -> Score {
        let (a, b) = (path[0], path[path.len() - 1]);
        let own = |at: Point| at == a || at == b;
        let inner = &path[1..path.len() - 1];
        let blocked = self
            .obstacles
            .iter()
            .any(|obstacle| !obstacle.contains(a) && !obstacle.contains(b) && path.windows(2).any(|leg| obstacle.blocks(leg[0], leg[1])))
            || self.contacts.iter().any(|at| !own(*at) && path_contains_point(path, *at))
            || others.iter().any(|other| {
                other.iter().any(|at| !own(*at) && path_contains_point(path, *at))
                    || inner.iter().any(|at| path_contains_point(other, *at))
                    || path.windows(2).any(|leg| other.windows(2).any(|run| heal::overlap(leg, run).is_some()))
            });
        Score::new(blocked, others.iter().map(|other| crossings(path, other)).sum(), bends(path), length(path))
    }
}

/// Grid lines within the margin around `low..=high`; when capped, those nearest the middle.
fn channels(low: i64, high: i64) -> impl Iterator<Item = i64> {
    let snap = |value: i64| value.div_euclid(GRID_NM);
    let (first, last) = (snap(low) - CHANNEL_MARGIN, snap(high) + CHANNEL_MARGIN);
    let middle = (first + last) / 2;
    let (first, last) = (first.max(middle - MAX_CHANNELS / 2), last.min(middle + MAX_CHANNELS / 2));
    (first..=last).map(|line| line.saturating_mul(GRID_NM))
}

/// Straight runs, elbows and one-channel detours between two points, in a fixed order.
/// Every candidate has nonzero legs that alternate between horizontal and vertical.
fn candidates(a: Point, b: Point, current: &[Point]) -> Vec<Vec<Point>> {
    let point = |x_nm, y_nm| Point { x_nm, y_nm };
    let mut routes = vec![vec![], vec![point(b.x_nm, a.y_nm)], vec![point(a.x_nm, b.y_nm)]];
    if let Some((low, high)) = bounds(current.iter().copied()) {
        routes.extend(channels(low.x_nm, high.x_nm).map(|x| vec![point(x, a.y_nm), point(x, b.y_nm)]));
        routes.extend(channels(low.y_nm, high.y_nm).map(|y| vec![point(a.x_nm, y), point(b.x_nm, y)]));
    }
    routes
        .into_iter()
        .filter(|inner| inner.iter().all(valid_point))
        .map(|inner| std::iter::once(a).chain(inner).chain(std::iter::once(b)).collect::<Vec<_>>())
        .filter(|path| {
            path.windows(2).all(|leg| leg[0] != leg[1] && (leg[0].x_nm == leg[1].x_nm || leg[0].y_nm == leg[1].y_nm))
                && path.windows(3).all(|run| (run[0].x_nm == run[1].x_nm) != (run[1].x_nm == run[2].x_nm))
        })
        .collect()
}

impl Project {
    /// Plan a cleanup of the selected wires on this sheet without changing anything. Each
    /// wire keeps its endpoints and takes the best route among its current one, a straight
    /// run, the two elbows and one-channel detours on the grid: clear of symbols, sheet
    /// symbols, labels and other wires' contacts first, then the lowest weighted cost of
    /// crossings, bends and length. A wire with no better route keeps its drawing. Unselected
    /// wires are fixed. The selection must name one to [`MAX_WIRES`] existing wires and
    /// nothing else; a selection in a region too dense to compare within the work bound is
    /// refused as a whole.
    pub fn wire_cleanup(&self, items: &Selection) -> Result<Cleanup, CommandError> {
        if !items.symbols.is_empty() || !items.net_labels.is_empty() || !items.no_connects.is_empty() || items.wires.is_empty() {
            return Err(CommandError::InvalidParams("select one or more wires to clean up".into()));
        }
        if items.wires.len() > MAX_WIRES {
            return Err(CommandError::CapExceeded(format!("clean up at most {MAX_WIRES} wires at once")));
        }
        let mut selected = BTreeSet::new();
        for id in &items.wires {
            let wire =
                self.wires.iter().find(|wire| wire.id() == *id).ok_or_else(|| CommandError::NotFound("selected wire does not exist".into()))?;
            selected.insert(wire.ordinal);
        }
        let original: Vec<Vec<Point>> = self.wires.iter().map(|wire| self.wire_path(wire)).collect();
        let chosen: Vec<usize> = self.wires.iter().enumerate().filter(|(_, wire)| selected.contains(&wire.ordinal)).map(|(index, _)| index).collect();
        let scene = Scene::new(self);
        let mut routes = original.clone();
        let mut budget = WORK_BUDGET;
        for _ in 0..PASSES {
            let mut changed = false;
            for &index in &chosen {
                let current = &routes[index];
                if current.len() < 2 || current[0] == current[current.len() - 1] {
                    continue;
                }
                // Only routes reaching into the box of every candidate can touch or cross one.
                let reach = bounds(current.iter().copied()).map(|(low, high)| {
                    let margin = (CHANNEL_MARGIN + 1) * GRID_NM;
                    (
                        Point { x_nm: low.x_nm.saturating_sub(margin), y_nm: low.y_nm.saturating_sub(margin) },
                        Point { x_nm: high.x_nm.saturating_add(margin), y_nm: high.y_nm.saturating_add(margin) },
                    )
                });
                let near = |route: &[Point]| {
                    bounds(route.iter().copied()).zip(reach).is_some_and(|((low, high), (min, max))| {
                        low.x_nm <= max.x_nm && high.x_nm >= min.x_nm && low.y_nm <= max.y_nm && high.y_nm >= min.y_nm
                    })
                };
                let others: Vec<&[Point]> = routes
                    .iter()
                    .enumerate()
                    .filter(|(other, route)| *other != index && route.len() >= 2 && near(route))
                    .map(|(_, route)| route.as_slice())
                    .collect();
                let kept = scene.score(current, &others);
                let candidates = candidates(current[0], current[current.len() - 1], current);
                budget = budget
                    .checked_sub(candidates.len().saturating_mul(others.len()))
                    .ok_or_else(|| CommandError::Unavailable("too many wires near this selection to clean up at once; select fewer wires".into()))?;
                // The current route wins ties, then the earliest candidate.
                let best = candidates
                    .into_iter()
                    .map(|path| (scene.score(&path, &others), path))
                    .filter(|(score, _)| !score.blocked && *score < kept)
                    .min_by_key(|(score, _)| *score);
                if let Some((_, path)) = best {
                    routes[index] = path;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        let totals = |routes: &[Vec<Point>]| {
            let mut totals = Totals::default();
            for &index in &chosen {
                totals.bends += bends(&routes[index]);
                totals.length_nm += length(&routes[index]);
                // A crossing of two selected wires is counted once, from the later wire.
                totals.crossings += routes
                    .iter()
                    .enumerate()
                    .filter(|(other, _)| *other != index && (*other < index || !selected.contains(&self.wires[*other].ordinal)))
                    .map(|(_, other)| crossings(&routes[index], other))
                    .sum::<usize>();
            }
            totals
        };
        let wires = chosen
            .iter()
            .filter(|&&index| routes[index] != original[index])
            .map(|&index| Reroute { wire: self.wires[index].ordinal, before: original[index].clone(), after: routes[index].clone() })
            .collect();
        Ok(Cleanup { before: totals(&original), after: totals(&routes), wires })
    }

    /// Apply a cleanup planned on this same project: each listed wire takes its new bends.
    /// Endpoints, identities, junctions and connectivity are untouched. A wire the plan
    /// names that no longer exists is refused.
    pub fn with_wire_cleanup(&self, cleanup: &Cleanup) -> Result<Project, CommandError> {
        let mut next = self.clone();
        let wires = std::sync::Arc::make_mut(&mut next.sheet_mut().wires);
        for reroute in &cleanup.wires {
            let wire: &mut Wire = wires
                .iter_mut()
                .find(|wire| wire.ordinal == reroute.wire)
                .ok_or_else(|| CommandError::NotFound("selected wire does not exist".into()))?;
            wire.bends = Some(interior(&reroute.after)?);
        }
        Ok(next)
    }
}

impl Session {
    /// Report the cleanup of the selected wires at the current revision. Nothing is cached
    /// and nothing changes: revision, history and project documents stay as they were.
    pub(crate) fn preview_wire_cleanup(&self, params: &Value) -> Result<Value, CommandError> {
        let project = self.open_project()?;
        let target = project.resolve_sheet(params.get("sheet").and_then(Value::as_str))?;
        let view = project.for_sheet(target).ok_or_else(|| CommandError::NotFound("unknown sheet ID".into()))?;
        let items = Selection::parse(&params["items"]).ok_or_else(|| CommandError::InvalidParams("invalid wire selection".into()))?;
        Ok(json!({"revision": self.revision(), "wire_cleanup": view.wire_cleanup(&items)?}))
    }

    /// Reroute the selected wires exactly as [`PREVIEW`] reports at the caller's revision, as
    /// one undo step. A selection with nothing to improve leaves revision and history alone.
    pub(crate) fn clean_up_wires(&mut self, params: &Value) -> Result<(), CommandError> {
        self.check_revision(params)?;
        let items = Selection::parse(&params["items"]).ok_or_else(|| CommandError::InvalidParams("invalid wire selection".into()))?;
        let project = self.open_project()?;
        let cleanup = project.wire_cleanup(&items)?;
        if cleanup.wires.is_empty() {
            return Ok(());
        }
        let next = project.with_wire_cleanup(&cleanup)?;
        self.change(next)
    }
}

#[cfg(test)]
mod tests;
