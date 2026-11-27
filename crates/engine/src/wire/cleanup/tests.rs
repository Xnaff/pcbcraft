use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::{DRAW_WIRE, PLACE_SYMBOL, Session, net_labels, wire::path_contains_point};

/// Build a model point in schematic grid units.
fn p(x: i64, y: i64) -> Point {
    Point { x_nm: x * GRID_NM, y_nm: y * GRID_NM }
}

/// Execute a fixture edit against the session's current revision.
fn run(s: &mut Session, command: &str, mut params: Value) {
    params["expected_revision"] = json!(s.revision());
    s.apply(command, &params).unwrap();
}

/// Open an empty project for command-driven cleanup fixtures.
fn session() -> Session {
    let mut s = Session::default();
    s.apply("project.new", &json!({"name":"Cleanup"})).unwrap();
    s
}

/// Draw a free-ended wire exactly as authored, without healing its jogs, and return its ID.
fn draw(s: &mut Session, from: Point, bends: &[Point], to: Point) -> String {
    run(s, DRAW_WIRE, json!({"from":from,"to":to,"bends":bends,"auto_heal":false}));
    s.project().unwrap().wires().last().unwrap().id()
}

/// Select only wires, as the cleanup commands require.
fn wires(ids: &[&str]) -> Value {
    json!({"symbols":[],"wires":ids})
}

/// The current drawn route of one wire.
fn route(s: &Session, id: &str) -> Vec<Point> {
    let project = s.project().unwrap();
    project.wire_path(project.wires().iter().find(|wire| wire.id() == id).unwrap())
}

/// A hand-drawn block: a jogged wire straightens, a short detour removes a crossing, and the
/// unselected wire stays fixed. The preview reports the same result that apply commits, the
/// netlist is unchanged, undo restores the drawing in one step and a second run is a no-op.
#[test]
fn jogs_and_a_crossing_clean_up_around_a_fixed_wire() {
    let mut s = session();
    let jogged = draw(&mut s, p(0, 0), &[p(4, 0), p(4, 2), p(6, 2), p(6, 0)], p(10, 0));
    let fixed = draw(&mut s, p(5, 8), &[], p(5, 14));
    let crossing = draw(&mut s, p(0, 10), &[], p(10, 10));
    let before = s.snapshot().unwrap();
    let revision = s.revision();
    let nets = s.project().unwrap().connectivity().nets.clone();
    let items = wires(&[&jogged, &crossing, &fixed]);

    let preview = s.execute(PREVIEW, &json!({"items":wires(&[&jogged, &crossing])})).unwrap();
    assert_eq!(s.revision(), revision);
    assert!(Arc::ptr_eq(&before, &s.snapshot().unwrap()));
    let report = &preview["wire_cleanup"];
    assert_eq!(report["before"], json!({"bends":4,"crossings":1,"length_nm":24 * GRID_NM}));
    assert_eq!(report["after"], json!({"bends":2,"crossings":0,"length_nm":26 * GRID_NM}));
    assert_eq!(report["wires"].as_array().unwrap().iter().map(|wire| wire["id"].as_str().unwrap()).collect::<Vec<_>>(), [&jogged, &crossing]);
    assert_eq!(report["wires"][0]["before"], json!([p(0, 0), p(4, 0), p(4, 2), p(6, 2), p(6, 0), p(10, 0)]));

    run(&mut s, CLEAN_UP, json!({"items":wires(&[&jogged, &crossing])}));
    assert_eq!(s.revision(), revision + 1);
    assert_eq!(route(&s, &jogged), [p(0, 0), p(10, 0)]);
    assert_eq!(route(&s, &crossing), [p(0, 10), p(0, 7), p(10, 7), p(10, 10)]);
    assert_eq!(json!(route(&s, &crossing)), report["wires"][1]["after"]);
    assert_eq!(route(&s, &fixed), [p(5, 8), p(5, 14)]);
    assert_eq!(s.project().unwrap().connectivity().nets, nets);
    assert_eq!(s.project().unwrap().junctions().collect::<Vec<_>>(), before.junctions().collect::<Vec<_>>());

    let cleaned = s.revision();
    run(&mut s, CLEAN_UP, json!({"items":items}));
    assert_eq!(s.revision(), cleaned);
    s.apply("edit.undo", &Value::Null).unwrap();
    assert!(Arc::ptr_eq(&before, &s.snapshot().unwrap()));
}

/// A wire drawn through a symbol or through a label's flag is rerouted around it, without
/// passing over the symbol's pins or the label's anchor.
#[test]
fn routes_leave_symbols_and_labels() {
    let mut s = session();
    run(&mut s, PLACE_SYMBOL, json!({"definition":"resistor","x_nm":p(20, 0).x_nm,"y_nm":0}));
    let pins = s.project().unwrap().symbols()[0].pins();
    let across = if pins[0].y_nm == pins[1].y_nm { [p(20, -6), p(20, 6)] } else { [p(14, 0), p(26, 0)] };
    let through_symbol = draw(&mut s, across[0], &[], across[1]);
    run(&mut s, net_labels::ADD, json!({"name":"VCC","style":"global","x_nm":0,"y_nm":p(0, 20).y_nm}));
    let through_label = draw(&mut s, p(2, 17), &[], p(2, 23));

    run(&mut s, CLEAN_UP, json!({"items":wires(&[&through_symbol, &through_label])}));
    let rerouted = route(&s, &through_symbol);
    assert_eq!(rerouted.len(), 4);
    assert!(!path_contains_point(&rerouted, p(20, 0)) && pins.iter().all(|pin| !path_contains_point(&rerouted, *pin)));
    assert_eq!(route(&s, &through_label), [p(2, 17), p(4, 17), p(4, 23), p(2, 23)]);
}

/// Refusals change nothing: a selection with anything but wires, an empty one, an unknown
/// wire and a stale revision.
#[test]
fn invalid_cleanups_change_nothing() {
    let mut s = session();
    let wire = draw(&mut s, p(0, 0), &[p(0, 3), p(4, 3)], p(4, 0));
    let before = s.snapshot().unwrap();
    let revision = s.revision();
    for items in [json!({"symbols":["symbol-9"],"wires":[wire]}), json!({"symbols":[],"wires":[]}), wires(&["wire-999"])] {
        assert!(s.apply(CLEAN_UP, &json!({"items":items,"expected_revision":revision})).is_err());
        assert!(s.execute(PREVIEW, &json!({"items":items})).is_err());
    }
    assert!(matches!(
        s.apply(CLEAN_UP, &json!({"items":wires(&[&wire]),"expected_revision":revision + 1})),
        Err(CommandError::StaleRevision { current }) if current == revision
    ));
    assert!(Arc::ptr_eq(&before, &s.snapshot().unwrap()));
    assert_eq!(s.revision(), revision);
}

/// Label boxes follow the canvas glyphs: a global flag extends along its rotation and a
/// local name always reads horizontally one grid from its anchor.
#[test]
fn label_boxes_follow_style_and_rotation() {
    let label = |style, rotation| Label { id: 1, name: "ABCDE".into(), style, at: p(10, 10), rotation, text: None };
    let flag = label_box(&label(Style::Global, 0));
    assert_eq!((flag.low.x_nm, flag.high.x_nm), (p(10, 0).x_nm, p(10, 0).x_nm + GRID_NM * 44 / 10));
    assert!(flag.blocks(p(12, 8), p(12, 12)) && !flag.blocks(p(9, 8), p(9, 12)) && !flag.blocks(p(10, 8), p(10, 12)));
    let turned = label_box(&label(Style::Global, 90));
    assert!(turned.blocks(p(8, 8), p(12, 8)) && !turned.blocks(p(8, 12), p(12, 12)));
    let name = label_box(&label(Style::Local, 180));
    assert!(name.blocks(p(8, 10), p(8, 12)) && !name.blocks(p(11, 10), p(11, 12)) && !name.blocks(p(0, 10), p(20, 10)));
}
