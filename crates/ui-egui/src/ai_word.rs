//! "Word", the AI assistant: a round button at the bottom-right of the window that opens a
//! small chat panel. Describe a document by typing or speaking; optionally research it first.

use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use egui::{Align2, Color32, Pos2, Sense, vec2};

use crate::theme::Tokens;
use crate::{WordApp, ai_format, ai_net, ai_plan, ai_research, ai_voice};

/// A finished request: the plan to build and what to tell the user.
struct Done {
    plan: ai_plan::Plan,
    label: String,
    summary: String,
}

#[derive(Default)]
pub struct State {
    open: bool,
    input: String,
    /// (from the user?, text)
    messages: Vec<(bool, String)>,
    /// `None` = Auto (fallback through every model); `Some(i)` = pinned to `ai_net::CANDIDATES[i]`.
    model: Option<usize>,
    /// Search the web and scholarly sources first, and cite them.
    research: bool,
    /// Put spoken text in the box instead of sending it straight away.
    review_voice: bool,
    busy: Option<Receiver<Result<Done, String>>>,
    listening: Option<ai_voice::Listening>,
        /// "Format document" was pressed; `show` starts it (it needs the document).
    format_request: Option<String>,
    format_busy: Option<Receiver<Result<ai_format::Proposal, String>>>,
    /// The options on offer.
    proposal: Option<ai_format::Proposal>,
    /// The option the user picked; `show` applies it.
    pick: Option<usize>,
}

pub fn show(app: &mut WordApp, ctx: &egui::Context) {
    let t = Tokens::get(ctx);
    let mut st = std::mem::take(&mut app.ai);
    poll(app, ctx, &mut st);
    launcher(ctx, &t, &mut st);
    if st.open {
        panel(ctx, &t, &mut st);
    }
    if let Some(request) = st.format_request.take() {
        start_format(ctx, &mut st, &app.session, request);
    }
    if let Some(i) = st.pick.take() {
        apply_choice(app, &mut st, i);
    }
    app.ai = st;
}

fn sparkle(p: &egui::Painter, c: Pos2, r: f32, color: Color32) {
    let mut mesh = egui::Mesh::default();
    mesh.colored_vertex(c, color);
    for i in 0..8 {
        let ang = i as f32 * std::f32::consts::FRAC_PI_4 - std::f32::consts::FRAC_PI_2;
        let rad = if i % 2 == 0 { r } else { r * 0.28 };
        mesh.colored_vertex(c + vec2(ang.cos() * rad, ang.sin() * rad), color);
    }
    for i in 0..8u32 {
        mesh.add_triangle(0, 1 + i, 1 + (i + 1) % 8);
    }
    p.add(egui::Shape::mesh(mesh));
}

fn launcher(ctx: &egui::Context, t: &Tokens, st: &mut State) {
    egui::Area::new(egui::Id::new("word-ai-launcher")).order(egui::Order::Foreground).anchor(Align2::RIGHT_BOTTOM, vec2(-22.0, -46.0)).show(ctx, |ui| {
        let (rect, resp) = ui.allocate_exact_size(vec2(50.0, 50.0), Sense::click());
        let fill = if resp.hovered() { t.accent.gamma_multiply(0.85) } else { t.accent };
        let p = ui.painter();
        p.circle_filled(rect.center() + vec2(0.0, 2.0), 25.0, Color32::from_black_alpha(50));
        p.circle_filled(rect.center(), 25.0, fill);
        sparkle(p, rect.center() + vec2(-2.0, 2.0), 13.0, Color32::WHITE);
        sparkle(p, rect.center() + vec2(9.0, -10.0), 5.0, Color32::WHITE);
        if resp.on_hover_text("Word: your AI writing assistant").clicked() {
            st.open = !st.open;
        }
    });
}

/// A microphone button drawn in code; red while recording.
fn mic_button(ui: &mut egui::Ui, t: &Tokens, recording: bool, enabled: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(36.0, 26.0), if enabled { Sense::click() } else { Sense::hover() });
    let p = ui.painter();
    let bg = if recording {
        t.red
    } else if enabled && resp.hovered() {
        t.hover
    } else {
        Color32::TRANSPARENT
    };
    p.rect_filled(rect, 5.0, bg);
    p.rect_stroke(rect, 5.0, egui::Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
    let ink = if recording {
        Color32::WHITE
    } else if enabled {
        t.icon
    } else {
        t.text_disabled
    };
    let c = rect.center();
    let stroke = egui::Stroke::new(1.6, ink);
    p.rect_filled(egui::Rect::from_center_size(c + vec2(0.0, -3.5), vec2(6.0, 10.0)), 3.0, ink);
    let arc: Vec<Pos2> = (0..=12)
        .map(|i| {
            let a = std::f32::consts::PI * (i as f32 / 12.0);
            c + vec2(a.cos() * 6.5, 0.5 + a.sin() * 5.5)
        })
        .collect();
    p.add(egui::Shape::line(arc, stroke));
    p.line_segment([c + vec2(0.0, 6.0), c + vec2(0.0, 9.0)], stroke);
    p.line_segment([c + vec2(-3.5, 9.0), c + vec2(3.5, 9.0)], stroke);
    resp
}

fn panel(ctx: &egui::Context, t: &Tokens, st: &mut State) {
    let mut open = true;
    let mut send: Option<String> = None;
    let mut mic = false;
    let mut format = false;
    let mut pick: Option<usize> = None;
    let mut dismiss = false;
    egui::Window::new("Word")
        .id(egui::Id::new("word-ai-panel"))
        .open(&mut open)
        .collapsible(false)
        .resizable(true)
        .default_width(380.0)
        .anchor(Align2::RIGHT_BOTTOM, vec2(-22.0, -108.0))
        .show(ctx, |ui| {
            if !ai_net::any_key() {
                ui.label(egui::RichText::new("No API key found. Set GEMINI_API_KEY or GROQ_API_KEY, then restart WordCraft.").color(t.red).small());
            }
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Model").small().color(t.text_dim));
                let current = st.model.and_then(|i| ai_net::CANDIDATES.get(i)).map_or("Auto (falls back)", |c| c.label);
                egui::ComboBox::from_id_salt("word-ai-model").selected_text(current).show_ui(ui, |ui| {
                    ui.selectable_value(&mut st.model, None, "Auto (falls back)");
                    for (i, c) in ai_net::CANDIDATES.iter().enumerate() {
                        ui.selectable_value(&mut st.model, Some(i), c.label);
                    }
                });
            });
            ui.checkbox(&mut st.research, "Research with sources")
                .on_hover_text("Searches the web and scholarly databases first, cites real sources by number and flags claims it can't support. Slower.");
            ui.separator();
            egui::ScrollArea::vertical().max_height(300.0).auto_shrink([false, true]).stick_to_bottom(true).show(ui, |ui| {
                if st.messages.is_empty() {
                    ui.label(
                        egui::RichText::new("Type or speak what you want, e.g. \"Create a professional document on AI agents for tomorrow's meeting.\" Or press Format document to tidy the one you have (you can type a style first, e.g. \"APA 7 student paper\").")
                            .color(t.text_dim),
                    );
                }
                for (mine, text) in &st.messages {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new(if *mine { "You" } else { "Word" }).small().strong());
                    ui.label(text);
                }
                if let Some(l) = &st.listening {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        let msg = if l.stopping { "Transcribing…" } else { "Listening… press the mic again when you're done" };
                        ui.label(egui::RichText::new(msg).color(t.text_dim));
                    });
                }
                if st.busy.is_some() {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        let msg = if st.research { "Word is researching and writing…" } else { "Word is writing…" };
                        ui.label(egui::RichText::new(msg).color(t.text_dim));
                    });
                }
                if st.format_busy.is_some() {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(egui::RichText::new("Word is choosing a layout…").color(t.text_dim));
                    });
                }
            });
            if let Some(p) = &st.proposal {
                ui.separator();
                ui.label(egui::RichText::new("Pick a layout").small().strong());
                for (i, c) in p.choices.iter().enumerate() {
                    ui.horizontal_wrapped(|ui| {
                        if ui.button(format!("Option {} · {}", i + 1, c.category.label())).clicked() {
                            pick = Some(i);
                        }
                        ui.label(egui::RichText::new(c.describe()).small().color(t.text_dim));
                    });
                }
                if ui.small_button("Dismiss").clicked() {
                    dismiss = true;
                }
            }
            ui.separator();
            let r = ui.add(egui::TextEdit::multiline(&mut st.input).desired_rows(3).desired_width(f32::INFINITY).hint_text("What should I write?"));
            let ctrl_enter = r.has_focus() && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter));
            ui.horizontal(|ui| {
                let listening = st.listening.is_some();
                let stopping = st.listening.as_ref().is_some_and(|l| l.stopping);
                let can_talk = !stopping && (listening || st.busy.is_none());
                let tip = if listening { "Stop and transcribe" } else { "Speak your request" };
                if mic_button(ui, t, listening, can_talk).on_hover_text(tip).clicked() && can_talk {
                    mic = true;
                }
                let can = st.busy.is_none() && !listening && !st.input.trim().is_empty();
                if ui.add_enabled(can, egui::Button::new("Create document")).clicked() || (can && ctrl_enter) {
                    send = Some(st.input.trim().to_string());
                }
                let can_format = st.format_busy.is_none() && !listening;
                if ui
                    .add_enabled(can_format, egui::Button::new("Format document"))
                    .on_hover_text("Offers two or three layouts for the whole document. Type a style first (e.g. APA 7) if you have one. Nothing changes until you pick.")
                    .clicked()
                {
                    format = true;
                }
                if ui.small_button("Clear chat").clicked() && st.busy.is_none() {
                    st.messages.clear();
                }
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut st.review_voice, "Review voice text before sending");
                ui.label(egui::RichText::new("Ctrl+Enter sends").small().color(t.text_dim));
            });
        });
    if !open {
        st.open = false;
    }
    if mic {
        toggle_mic(st);
    }
    if format {
        st.format_request = Some(st.input.trim().to_string());
    }
    if dismiss {
        st.proposal = None;
    }
    if pick.is_some() {
        st.pick = pick;
    }
    if let Some(prompt) = send {
        start(ctx, st, prompt);
    }
}

fn toggle_mic(st: &mut State) {
    match st.listening.as_mut() {
        Some(l) => l.stop(),
        None => match ai_voice::start() {
            Ok(l) => st.listening = Some(l),
            Err(e) => st.messages.push((false, format!("Microphone: {e}"))),
        },
    }
}

/// The background work: plain writing, or research then writing.
fn work(prompt: &str, only: Option<usize>, research: bool) -> Result<Done, String> {
    if research {
        let o = ai_research::run(prompt, only)?;
        return Ok(Done { plan: o.plan, label: o.label, summary: o.summary });
    }
    let reply = ai_net::ask(ai_plan::SYSTEM_PROMPT, prompt, only, |text| ai_plan::parse_plan(text).is_ok())?;
    let plan = ai_plan::parse_plan(&reply.text)?;
    Ok(Done { plan, label: reply.label, summary: String::new() })
}

fn start(ctx: &egui::Context, st: &mut State, prompt: String) {
    st.messages.push((true, prompt.clone()));
    st.input.clear();
    let (tx, rx) = channel();
    let wake = ctx.clone();
    let only = st.model;
    let research = st.research;
    let spawned = std::thread::Builder::new().name("word-ai".into()).spawn(move || {
        let r = work(&prompt, only, research);
        let _ = tx.send(r);
        wake.request_repaint();
    });
    match spawned {
        Ok(_) => st.busy = Some(rx),
        Err(e) => st.messages.push((false, format!("Couldn't start the request: {e}"))),
    }
}

fn poll(app: &mut WordApp, ctx: &egui::Context, st: &mut State) {
    // Voice: a transcript (or an error) arrived.
    if let Some(l) = &st.listening {
        match l.poll() {
            None => ctx.request_repaint_after(Duration::from_millis(150)),
            Some(result) => {
                st.listening = None;
                match result {
                    Err(e) => st.messages.push((false, format!("Voice input failed: {e}"))),
                    Ok(text) => {
                        let text = text.trim().to_string();
                        if text.is_empty() {
                            st.messages.push((false, "I didn't hear anything. Press the mic and try again.".into()));
                        } else if st.review_voice || st.busy.is_some() {
                            if !st.input.trim().is_empty() {
                                st.input.push(' ');
                            }
                            st.input.push_str(&text);
                        } else {
                            start(ctx, st, text);
                        }
                    }
                }
                ctx.request_repaint();
            }
        }
    }
        // Formatting: the options are ready.
    if let Some(rx) = &st.format_busy {
        match rx.try_recv() {
            Ok(result) => {
                st.format_busy = None;
                match result {
                    Ok(p) => {
                        st.messages.push((false, ai_format::intro(&p)));
                        st.proposal = Some(p);
                    }
                    Err(e) => st.messages.push((false, format!("I couldn't work out a formatting plan.\n{e}"))),
                }
                ctx.request_repaint();
            }
            Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(150)),
            Err(TryRecvError::Disconnected) => {
                st.format_busy = None;
                st.messages.push((false, "The request stopped unexpectedly.".into()));
            }
        }
    }
    // Writing: the document is ready.
    let Some(rx) = &st.busy else { return };
    match rx.try_recv() {
        Ok(result) => {
            st.busy = None;
            let msg = match result {
                Ok(done) => finish(app, done),
                Err(e) => format!("I couldn't create the document.\n{e}"),
            };
            st.messages.push((false, msg));
            ctx.request_repaint();
        }
        Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(150)),
        Err(TryRecvError::Disconnected) => {
            st.busy = None;
            st.messages.push((false, "The request stopped unexpectedly.".into()));
        }
    }
}

/// Build the plan into the document; the message to show.
fn finish(app: &mut WordApp, done: Done) -> String {
    match ai_plan::apply_plan(&mut app.session, &done.plan) {
        Ok(n) => {
            app.canvas.scroll_to_caret = true;
            let mut kind = done.plan.document_type.clone();
            if !done.plan.audience.is_empty() {
                kind = format!("{kind} for {}", done.plan.audience);
            }
            let mut m = format!("Done: {kind} ({n} blocks added at the end of the page).\n{}", done.plan.summary);
            if !done.summary.is_empty() {
                m.push_str("\n\n");
                m.push_str(&done.summary);
            }
            m.push_str(&format!("\n\nWritten by {}. Ctrl+Z undoes all of it.", done.label));
            m
        }
        Err(e) => format!("I couldn't build the document, and I undid what I had added.\n{e}"),
    }
}
/// Look at the document, then ask the models for formatting options on a worker thread.
fn start_format(ctx: &egui::Context, st: &mut State, session: &wordcraft_engine::Session, request: String) {
    let shown = if request.is_empty() { "Format my document".to_string() } else { format!("Format my document: {request}") };
    st.messages.push((true, shown));
    st.input.clear();
    st.proposal = None;
    let profile = ai_format::profile(session);
    let (tx, rx) = channel();
    let wake = ctx.clone();
    let only = st.model;
    let spawned = std::thread::Builder::new().name("word-ai-format".into()).spawn(move || {
        let r = ai_format::propose(&request, profile, only);
        let _ = tx.send(r);
        wake.request_repaint();
    });
    match spawned {
        Ok(_) => st.format_busy = Some(rx),
        Err(e) => st.messages.push((false, format!("Couldn't start the request: {e}"))),
    }
}

/// Apply the option the user picked. The options stay on offer so another can be tried.
fn apply_choice(app: &mut WordApp, st: &mut State, i: usize) {
    let Some(p) = st.proposal.clone() else { return };
    let Some(choice) = p.choices.get(i) else { return };
    let msg = match ai_format::apply(&mut app.session, choice) {
        Ok(done) => ai_format::done_message(&p, i, choice, &done),
        Err(e) => format!("I couldn't apply that layout, and I undid what I had changed.\n{e}"),
    };
    st.messages.push((false, msg));
}