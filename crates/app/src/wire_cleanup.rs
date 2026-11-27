//! "Clean Up Wires…" captures the selected schematic wires with their originating view and
//! revision, shows the engine's cleanup plan as before and after drawings with its bend and
//! crossing counts, and applies the same plan as one undo step. The engine owns the routing
//! and every check; this form only presents its answers.

use crate::{Dialog, PcbApp, ViewInput, schematic_canvas::Thumbnail};
use pcbcraft_chrome::shell;
use pcbcraft_engine::{
    SheetTarget,
    schematic::Point,
    selection::Selection,
    wire::cleanup::{CLEAN_UP, Cleanup},
};
use serde_json::json;

/// Height of each of the two drawings.
const DRAWING_HEIGHT: f32 = 200.0;

/// The engine's plan with a drawing of the sheet before and after it.
#[derive(Debug)]
struct Review {
    cleanup: Cleanup,
    selected: usize,
    /// `None` when the sheet is too large to draw; the counts are still shown.
    before: Option<Thumbnail>,
    after: Option<Thumbnail>,
}

#[derive(Debug)]
pub(super) struct Edit {
    pub(super) source: ViewInput,
    sheet: SheetTarget,
    items: Selection,
    /// The engine's answer at `previewed`: its plan, or why it refuses.
    preview: Result<Review, String>,
    /// The revision the preview was made for; a newer revision previews again.
    previewed: Option<u64>,
    error: Option<String>,
}

impl Edit {
    /// Cleaning up is offered only for a plan that redraws at least one wire.
    fn can_clean_up(&self) -> bool {
        self.preview.as_ref().is_ok_and(|review| !review.cleanup.wires.is_empty())
    }
}

/// Whether the selection is one the cleanup commands accept: wires and nothing else.
pub(super) fn selects_only_wires(items: &Selection) -> bool {
    !items.wires.is_empty() && items.symbols.is_empty() && items.net_labels.is_empty() && items.no_connects.is_empty()
}

/// "1 wire", "3 wires": counts in the summary line.
fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

impl PcbApp {
    /// Open the review for the active view's selected wires. Nothing changes until Clean Up.
    pub(super) fn begin_wire_cleanup(&mut self) {
        self.begin_wire_cleanup_with_items(self.views.active.schematic.selection.clone());
    }

    /// Open the review for the supplied wires; a context action can name its own target. A
    /// selection with anything but wires, an open form or an active tool opens nothing.
    pub(super) fn begin_wire_cleanup_with_items(&mut self, items: Selection) {
        if !matches!(self.dialog, Dialog::None) || self.views.active.schematic.tool.is_some() || !selects_only_wires(&items) {
            return;
        }
        let Some(source) = self.views.active.input(&self.session) else {
            return;
        };
        let Some(project) = self.views.active.snapshot(&self.session) else {
            return;
        };
        self.dialog = Dialog::WireCleanup(Box::new(Edit {
            source,
            sheet: project.sheet_target(),
            items,
            preview: Err(String::new()),
            previewed: None,
            error: None,
        }));
    }

    /// Ask the engine what the cleanup would do at the current revision and prepare both
    /// drawings once. The plan comes from the same function as the MCP preview command.
    fn refresh_wire_cleanup(&mut self, edit: &mut Edit) {
        let revision = self.session.revision();
        if edit.previewed == Some(revision) {
            return;
        }
        edit.previewed = Some(revision);
        let project = self.session.project().and_then(|project| project.for_sheet(edit.sheet));
        edit.preview = match project {
            None => Err("This sheet no longer exists.".into()),
            Some(project) => project
                .wire_cleanup(&edit.items)
                .and_then(|cleanup| Ok((project.with_wire_cleanup(&cleanup)?, cleanup)))
                .map_err(|error| error.to_string())
                .map(|(cleaned, cleanup)| Review {
                    selected: edit.items.wires.len(),
                    before: Thumbnail::new(&project),
                    after: Thumbnail::new(&cleaned),
                    cleanup,
                }),
        };
    }

    /// Render the review. Cancel and closing are inert; a stale review does not apply;
    /// engine refusals stay on screen.
    pub(super) fn wire_cleanup_dialog(&mut self, ctx: &egui::Context, mut edit: Box<Edit>) {
        self.refresh_wire_cleanup(&mut edit);
        let paper = self.preferences.paper();
        let (mut submit, mut cancel) = (false, false);
        let closing = shell::dialog_width(ctx, "wire_cleanup_dialog", "Clean Up Wires", 560.0, |ui| {
            match &edit.preview {
                Err(reason) => {
                    ui.label(shell::error(reason.as_str()));
                }
                Ok(review) if review.cleanup.wires.is_empty() => {
                    ui.label("Cleanup found no better route for the selected wires.");
                }
                Ok(review) => {
                    let (before, after) = (review.cleanup.before, review.cleanup.after);
                    ui.label(format!(
                        "Redraws {} of {}. Endpoints, junctions and other wires stay as they are.",
                        count(review.cleanup.wires.len(), "wire", "wires"),
                        count(review.selected, "selected wire", "selected wires")
                    ));
                    ui.label(format!("Bends: {} → {}", before.bends, after.bends));
                    ui.label(format!("Crossings: {} → {}", before.crossings, after.crossings));
                    let focus: Vec<Point> = review.cleanup.wires.iter().flat_map(|wire| wire.before.iter().chain(&wire.after).copied()).collect();
                    let routes = |after: bool| -> Vec<Vec<Point>> {
                        review.cleanup.wires.iter().map(|wire| if after { wire.after.clone() } else { wire.before.clone() }).collect()
                    };
                    ui.columns(2, |columns| {
                        for (column, (caption, drawing, routes)) in
                            columns.iter_mut().zip([("Before", &review.before, routes(false)), ("After", &review.after, routes(true))])
                        {
                            column.strong(caption);
                            let (rect, response) =
                                column.allocate_exact_size(egui::vec2(column.available_width(), DRAWING_HEIGHT), egui::Sense::hover());
                            response.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Image, true, format!("{caption} drawing")));
                            match drawing {
                                Some(drawing) => drawing.paint_focused(column.painter(), rect, paper, &focus, &routes),
                                None => {
                                    column.put(rect, egui::Label::new("This sheet is too large to draw here."));
                                }
                            }
                        }
                    });
                }
            }
            if let Some(error) = &edit.error {
                ui.label(shell::error(error.as_str()));
            }
            ui.horizontal(|ui| {
                submit = ui.add_enabled_ui(edit.can_clean_up(), |ui| pcbcraft_chrome::controls::button(ui, "Clean Up")).inner.clicked();
                cancel = pcbcraft_chrome::controls::button(ui, "Cancel").clicked();
            });
        });
        if submit && edit.can_clean_up() {
            if !self.views.view(edit.source.view()).is_some_and(|view| view.control_input(&self.session) == Some(edit.source)) {
                self.message = Some("View or project changed; choose Clean Up Wires again to continue.".into());
                return;
            }
            let params = json!({"items":edit.items,"sheet":edit.sheet.id(),"expected_revision":edit.source.revision});
            match self.session.apply(CLEAN_UP, &params) {
                Ok(_) => {
                    self.views.sync(&self.session);
                    self.message = None;
                    self.file_notice = None;
                    self.repaint_editors(ctx);
                }
                Err(error) => {
                    edit.error = Some(error.to_string());
                    self.dialog = Dialog::WireCleanup(edit);
                }
            }
        } else if !(closing || cancel) {
            self.dialog = Dialog::WireCleanup(edit);
        }
    }
}
