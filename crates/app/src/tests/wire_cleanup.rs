//! Cleaning up selected wires through its review: opening it from the Modify menu, the
//! before/after counts and drawings, applying as one undo step, Cancel, and a selection
//! with nothing to improve.
use super::*;
use pcbcraft_engine::{DRAW_WIRE, selection::Selection};

/// One free wire through the given bends, selected.
fn wire_harness(bends: &[(i64, i64)]) -> Harness<'static, PcbApp> {
    let p = |x: i64, y: i64| json!({"x_nm":x * 1_270_000,"y_nm":y * 1_270_000});
    let bends: Vec<_> = bends.iter().map(|(x, y)| p(*x, *y)).collect();
    let mut app = PcbApp::default();
    app.run("project.new", &json!({"name":"Cleanup"}));
    app.run(DRAW_WIRE, &json!({"from":p(0, 0),"to":p(10, 0),"bends":bends,"auto_heal":false,"expected_revision":app.session.revision()}));
    let wire = app.session.project().unwrap().wires()[0].id();
    let mut h = Harness::builder().with_size(egui::vec2(1400.0, 900.0)).with_max_steps(40).build_ui_state(|ui, app: &mut PcbApp| app.frame(ui), app);
    h.run();
    h.state_mut().select(Selection::one_wire(wire));
    h.run();
    h
}

/// Open the review through the Modify group's menu.
fn open(h: &mut Harness<'_, PcbApp>) {
    click(h, "Modify menu");
    click(h, "Clean Up Wires…");
    assert!(matches!(h.state().dialog, Dialog::WireCleanup(_)));
}

/// The review shows the bend and crossing counts with both drawings and changes nothing;
/// Clean Up redraws the wire as one undo step and keeps it selected.
#[test]
fn cleanup_review_shows_counts_and_applies_as_one_undo_step() {
    let mut h = wire_harness(&[(4, 0), (4, 2), (6, 2), (6, 0)]);
    let before = h.state().session.snapshot().unwrap();
    let revision = h.state().session.revision();
    open(&mut h);
    h.get_by_label("Redraws 1 wire of 1 selected wire. Endpoints, junctions and other wires stay as they are.");
    h.get_by_label("Bends: 4 → 0");
    h.get_by_label("Crossings: 0 → 0");
    h.get_by_label("Before drawing");
    h.get_by_label("After drawing");
    assert_eq!(h.state().session.revision(), revision);
    click(&mut h, "Clean Up");
    assert!(matches!(h.state().dialog, Dialog::None));
    let project = h.state().session.project().unwrap();
    assert_eq!(project.wire_path(&project.wires()[0]).len(), 2);
    assert_eq!(h.state().views.active.schematic.selection.wires.len(), 1);
    click(&mut h, "Undo");
    assert_same_content(&before, &h.state().session.snapshot().unwrap());
}

/// Cancel changes nothing, and a wire with no better route offers no Clean Up.
#[test]
fn cleanup_review_cancels_and_reports_nothing_to_improve() {
    let mut h = wire_harness(&[(4, 0), (4, 2), (6, 2), (6, 0)]);
    let before = h.state().session.snapshot().unwrap();
    open(&mut h);
    click(&mut h, "Cancel");
    assert!(matches!(h.state().dialog, Dialog::None));
    assert!(Arc::ptr_eq(&before, &h.state().session.snapshot().unwrap()));

    let mut h = wire_harness(&[]);
    open(&mut h);
    h.get_by_label("Cleanup found no better route for the selected wires.");
    assert!(h.get_by_label("Clean Up").accesskit_node().is_disabled());
}
