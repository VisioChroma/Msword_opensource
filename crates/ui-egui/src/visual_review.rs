//! Visual review of two document snapshots.

use egui::{Color32, Rect, Sense, TextureHandle, Ui, pos2, vec2};
use serde_json::json;
use wordcraft_doc::Document;
use wordcraft_engine::Session;

use crate::WordApp;

#[derive(Clone)]
struct Preview {
    left: TextureHandle,
    right: TextureHandle,
    highlighted: TextureHandle,
    left_pages: usize,
    right_pages: usize,
    changed_pixels: usize,
    objects_differ: bool,
    sections_differ: bool,
}

#[derive(Clone, Default)]
struct Cache {
    key: Option<(u64, usize)>,
    page: usize,
    highlight: bool,
    preview: Option<Preview>,
    error: String,
}

/// Red pixels indicate visible differences above a small rendering tolerance.
fn highlighted_pixels(a: &[u8], b: &[u8]) -> (Vec<u8>, usize) {
    let mut result = b.to_vec();
    let mut changed = 0usize;

    for ((old, new), output) in a.chunks_exact(4).zip(b.chunks_exact(4)).zip(result.chunks_exact_mut(4)) {
        let different = old.iter().take(3).zip(new.iter().take(3)).any(|(x, y)| x.abs_diff(*y) > 24);

        if different {
            output.copy_from_slice(&[255, 96, 96, 255]);
            changed += 1;
        }
    }

    (result, changed)
}

fn build_preview(a: &Document, b: &Document, page: usize, ctx: &egui::Context) -> Result<Preview, String> {
    // Both versions use the same export-layout and rendering defaults.
    let left_layout = Session::new(a.clone()).export_layout();
    let right_layout = Session::new(b.clone()).export_layout();

    if left_layout.pages.len().max(right_layout.pages.len()) > 200 {
        return Err("Visual review currently supports up to 200 pages.".into());
    }

    let left_page = left_layout.pages.get(page);
    let right_page = right_layout.pages.get(page);

    if left_page.is_none() && right_page.is_none() {
        return Err("This page does not exist in either snapshot.".into());
    }

    let width = left_page.map_or(0.0, |p| p.w).max(right_page.map_or(0.0, |p| p.w));
    let height = left_page.map_or(0.0, |p| p.h).max(right_page.map_or(0.0, |p| p.h));

    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 || width > 20_000.0 || height > 20_000.0 {
        return Err("The page dimensions are outside the preview limits.".into());
    }

    // Same scale and canvas size: moving an object remains visible as movement.
    let scale = (900.0 / width.max(height)).min(1.0);

    let left_image = left_page.map(|p| wordcraft_render::render_area(a, p, 0.0, 0.0, width, height, scale, &Default::default()));
    let right_image = right_page.map(|p| wordcraft_render::render_area(b, p, 0.0, 0.0, width, height, scale, &Default::default()));

    let reference = left_image.as_ref().or(right_image.as_ref()).ok_or_else(|| "There is no page to render.".to_string())?;

    let size = [reference.width as usize, reference.height as usize];
    let length = size
        .first()
        .copied()
        .and_then(|w| size.get(1).and_then(|h| w.checked_mul(*h)))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "The preview dimensions are too large.".to_string())?;

    if length > 8_000_000 {
        return Err("The rendered preview exceeds the memory limit.".into());
    }

    for image in [left_image.as_ref(), right_image.as_ref()].into_iter().flatten() {
        if [image.width as usize, image.height as usize] != size {
            return Err("The two preview canvases have different dimensions.".into());
        }
    }

    // A missing page is represented by an empty white canvas.
    let left_pixels = left_image.as_ref().map(|image| image.to_straight()).unwrap_or_else(|| vec![255; length]);
    let right_pixels = right_image.as_ref().map(|image| image.to_straight()).unwrap_or_else(|| vec![255; length]);

    if left_pixels.len() != length || right_pixels.len() != length {
        return Err("The renderer returned an unexpected pixel count.".into());
    }

    let (highlighted, changed_pixels) = highlighted_pixels(&left_pixels, &right_pixels);

    let texture =
        |name: &str, pixels: &[u8]| ctx.load_texture(name, egui::ColorImage::from_rgba_unmultiplied(size, pixels), egui::TextureOptions::LINEAR);

    Ok(Preview {
        left: texture("visual_review_baseline", &left_pixels),
        right: texture("visual_review_revised", &right_pixels),
        highlighted: texture("visual_review_changes", &highlighted),
        left_pages: left_layout.pages.len(),
        right_pages: right_layout.pages.len(),
        changed_pixels,
        objects_differ: wordcraft_engine::cmd::review::visual_objects_differ(a, b),
        sections_differ: a.sections() != b.sections(),
    })
}

fn draw_page(ui: &mut Ui, texture: &TextureHandle) {
    let native = texture.size_vec2();
    let width = ui.available_width().clamp(100.0, 600.0);
    let height = width * native.y / native.x.max(1.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());

    ui.painter().image(texture.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
}
pub fn show(app: &mut WordApp, ui: &mut Ui) {
    if !app.session.visual_review.open {
        return;
    }

    let ctx = ui.ctx().clone();
    let cache_id = egui::Id::new("visual_review_cache");
    let mut cache: Cache = ctx.data(|data| data.get_temp(cache_id)).unwrap_or_default();

    let mut open = true;
    let mut return_to_editor = false;

    egui::Window::new(tl!("Visual document review"))
        .id(egui::Id::new("visual_review_window"))
        .open(&mut open)
        .default_size(vec2(1000.0, 650.0))
        .resizable(true)
        .show(&ctx, |ui| {
            ui.label(tl!("Capture a baseline, edit or open another version, then compare."));

            ui.horizontal_wrapped(|ui| {
                let baseline_exists = app.session.visual_review.baseline.is_some();
                let capture_label = if baseline_exists { tl!("Replace baseline with current document") } else { tl!("Capture baseline") };

                if ui.button(capture_label).clicked() {
                    match app.run("review.visualBaseline", json!({})) {
                        Ok(_) => {
                            cache = Cache::default();
                        }
                        Err(e) => cache.error = e,
                    }
                }

                if ui.add_enabled(baseline_exists, egui::Button::new(tl!("Compare current document"))).clicked() {
                    match app.run("review.visualCompare", json!({})) {
                        Ok(_) => {
                            cache.page = 0;
                            cache.key = None;
                            cache.error.clear();
                        }
                        Err(e) => cache.error = e,
                    }
                }

                if ui.button(tl!("Clear snapshots")).clicked() {
                    match app.run("review.visualClear", json!({})) {
                        Ok(_) => cache = Cache::default(),
                        Err(e) => cache.error = e,
                    }
                }
            });

            ui.horizontal_wrapped(|ui| {
                if ui.button(tl!("Review current comments")).clicked() {
                    let _ = app.run("view.commentsPane", json!({"value": true}));
                    return_to_editor = true;
                }

                if ui.button(tl!("Review tracked changes")).clicked() {
                    crate::widgets::run_from_ribbon(app, "review.changes", json!({}));
                    return_to_editor = true;
                }
            });

            ui.label(tl!("Accept and Reject in the Review ribbon apply to tracked revisions, not to the red visual highlights."));

            let state = &app.session.visual_review;

            if let Some(stamp) = state.compared_stamp
                && stamp != (app.session.document_id(), app.session.rev())
            {
                ui.label(tl!("The working document has changed. Compare again to refresh these snapshots."));
            }

            if let (Some(a), Some(b)) = (&state.baseline, &state.compared) {
                let key = (state.generation, cache.page);
                if cache.key != Some(key) {
                    cache.key = Some(key);
                    match build_preview(a, b, cache.page, &ctx) {
                        Ok(preview) => {
                            cache.preview = Some(preview);
                            cache.error.clear();
                        }
                        Err(e) => {
                            cache.preview = None;
                            cache.error = e;
                        }
                    }
                }

                if let Some(preview) = cache.preview.clone() {
                    let total = preview.left_pages.max(preview.right_pages);
                    let mut next_page = cache.page;

                    ui.horizontal_wrapped(|ui| {
                        if ui.add_enabled(cache.page > 0, egui::Button::new(tl!("Previous Page"))).clicked() {
                            next_page = cache.page.saturating_sub(1);
                        }

                        ui.label(format!("{} / {}", cache.page + 1, total));

                        if ui.add_enabled(cache.page + 1 < total, egui::Button::new(tl!("Next Page"))).clicked() {
                            next_page = cache.page.saturating_add(1);
                        }

                        ui.checkbox(&mut cache.highlight, tl!("Highlight visual differences"));
                    });

                    if preview.left_pages != preview.right_pages {
                        ui.label(tl!("The page counts differ."));
                    }
                    if preview.objects_differ {
                        ui.label(tl!("Drawing-object definitions differ between the snapshots."));
                    }
                    if preview.sections_differ {
                        ui.label(tl!("Section structure or page settings differ between the snapshots."));
                    }

                    ui.label(if preview.changed_pixels == 0 {
                        tl!("No visual difference detected on this page at preview resolution.")
                    } else {
                        tl!("Visual differences detected on this page.")
                    });

                    egui::ScrollArea::both().id_salt("visual_review_pages").max_height(560.0).show(ui, |ui| {
                        ui.columns(2, |columns| {
                            for (index, column) in columns.iter_mut().enumerate() {
                                if index == 0 {
                                    column.label(tl!("Baseline snapshot"));
                                    if cache.page >= preview.left_pages {
                                        column.label(tl!("No page in this version"));
                                    }
                                    draw_page(column, &preview.left);
                                } else {
                                    column.label(tl!("Compared snapshot"));
                                    if cache.page >= preview.right_pages {
                                        column.label(tl!("No page in this version"));
                                    }
                                    let image = if cache.highlight { &preview.highlighted } else { &preview.right };
                                    draw_page(column, image);
                                }
                            }
                        });
                    });

                    if next_page != cache.page {
                        cache.page = next_page;
                        ctx.request_repaint();
                    }
                }
            } else {
                ui.label(tl!(
                    "Capture the baseline first. Close this window to edit or open the revised file, then reopen Visual Review and compare."
                ));
            }

            if !cache.error.is_empty() {
                ui.label(&cache.error);
            }
        });

    app.session.visual_review.open = open && !return_to_editor;
    ctx.data_mut(|data| data.insert_temp(cache_id, cache));
}
#[cfg(test)]
mod tests {
    use super::highlighted_pixels;

    #[test]
    fn visual_review_highlights_added_and_removed_pixels() {
        let white = [255, 255, 255, 255];
        let black = [0, 0, 0, 255];

        assert_eq!(highlighted_pixels(&white, &white).1, 0);
        assert_eq!(highlighted_pixels(&white, &black).1, 1);
        assert_eq!(highlighted_pixels(&black, &white).1, 1);

        let (pixels, _) = highlighted_pixels(&black, &white);
        assert_eq!(pixels, vec![255, 96, 96, 255]);
    }
}
