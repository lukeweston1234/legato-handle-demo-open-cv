//! The slicer's source of truth: lay out which slice each finger plays, on a
//! grid at the sample's tempo, and hear it straight away. Edits go to the
//! slicer through its handle, so you can tune by ear while playing.

use std::{path::PathBuf, sync::Arc};

use eframe::egui::{
    self, Align2, Color32, FontId, Key, Painter, Pos2, Rect, Response, Sense, Stroke, StrokeKind,
    Ui, pos2, vec2,
};
use legato::{
    LegatoFrontend,
    msg::{NodeMessage, ParamPayload, RtValue},
};

use crate::{
    bounds::{Bounds, Preset, Shared, Slice},
    gesture::PADS,
};

const FINGERS: [&str; PADS] = ["index", "middle", "ring", "pinky"];
const PAD_COLORS: [Color32; PADS] = [
    Color32::from_rgb(0xff, 0x8a, 0x3d),
    Color32::from_rgb(0x3d, 0xc8, 0xff),
    Color32::from_rgb(0xb0, 0x7c, 0xff),
    Color32::from_rgb(0x7c, 0xe0, 0x6a),
];
const WAVE: Color32 = Color32::from_gray(150);
const PLAYHEAD: Color32 = Color32::WHITE;
/// How close, in points, a press must be to an edge to grab it.
const GRAB: f32 = 8.0;
/// Height of each finger's lane above the close-up waveform.
const LANE: f32 = 20.0;

pub struct Viewer {
    frontend: LegatoFrontend,
    shared: Arc<Shared>,
    /// The clock node retuned when `follow_clock` is on.
    clock: String,
    peaks: Peaks,
    secs: f32,
    name: String,
    path: PathBuf,
    preset: Preset,
    saved: Option<Preset>,
    sent_bpm: Option<f32>,
    drag: Option<Drag>,
    /// The pad the waveform edits and the play button plays.
    selected: usize,
    status: String,
}

/// What a drag is moving. Positions are fractions of the sample.
#[derive(Debug, Clone, Copy)]
enum Drag {
    ViewStart,
    ViewEnd,
    /// Sweeping out a new view from here.
    NewView(f32),
    Pad(usize, PadDrag),
}

#[derive(Debug, Clone, Copy)]
enum PadDrag {
    Start,
    End,
    /// Sliding the whole slice, grabbed this far from its start.
    Move(f32),
    /// Sweeping out a new slice from here.
    New(f32),
}

pub struct ViewerArgs {
    pub frontend: LegatoFrontend,
    pub shared: Arc<Shared>,
    pub clock: String,
    /// The sample mixed to mono, at the graph's sample rate.
    pub mono: Vec<f32>,
    pub sample_rate: f32,
    pub sample: PathBuf,
    pub preset: Preset,
    /// Whether `preset` came from the file next to the sample.
    pub saved: bool,
}

/// Opens the viewer and blocks until it is closed.
pub fn run(args: ViewerArgs) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Marionette")
            .with_inner_size([1100.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "marionette",
        options,
        Box::new(|_| Ok(Box::new(Viewer::new(args)))),
    )
}

impl Viewer {
    fn new(a: ViewerArgs) -> Self {
        let name = a.sample.file_name().map_or_else(
            || a.sample.display().to_string(),
            |n| n.to_string_lossy().into(),
        );
        Self {
            frontend: a.frontend,
            shared: a.shared,
            clock: a.clock,
            secs: a.mono.len() as f32 / a.sample_rate,
            peaks: Peaks::new(&a.mono),
            name,
            path: Preset::path_for(&a.sample),
            saved: a.saved.then_some(a.preset),
            preset: a.preset,
            sent_bpm: None,
            drag: None,
            selected: 0,
            status: String::new(),
        }
    }

    fn save(&mut self) {
        match std::fs::write(&self.path, self.preset.to_text()) {
            Ok(()) => {
                self.saved = Some(self.preset);
                self.status = format!("saved {}", self.path.display());
            }
            Err(e) => self.status = format!("could not save {}: {e}", self.path.display()),
        }
    }

    /// Push edits to the audio thread, and retune the clock if it follows.
    fn sync(&mut self) {
        self.preset.bounds = self.preset.bounds.sanitized();
        if self.shared.bounds() != self.preset.bounds {
            self.shared.set_bounds(self.preset.bounds);
        }

        let bpm = self.preset.bpm(self.secs).clamp(20.0, 400.0);
        if self.preset.follow_clock && self.sent_bpm.is_none_or(|b| (b - bpm).abs() > 0.01) {
            let msg = NodeMessage::SetParam(ParamPayload {
                param_name: "bpm",
                value: RtValue::F32(bpm),
            });
            match self.frontend.send_node_msg(&self.clock, msg) {
                Ok(()) => self.sent_bpm = Some(bpm),
                // A full queue clears within a block; try again next frame.
                Err(legato::FrontendError::QueueFull) => {}
                Err(e) => {
                    self.status = format!("can't retune '{}': {e}", self.clock);
                    self.preset.follow_clock = false;
                }
            }
        }
    }

    fn header(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.heading(&self.name);
            ui.label(format!("{:.2} s", self.secs));
            let dirty = self.saved != Some(self.preset);
            if ui
                .add_enabled(dirty, egui::Button::new("Save"))
                .on_hover_text(format!("{} (ctrl+s)", self.path.display()))
                .clicked()
            {
                self.save();
            }
            if dirty {
                ui.weak("unsaved");
            }
            ui.weak(&self.status);
        });
    }

    /// Select `pad` and play it once.
    fn audition(&mut self, pad: usize) {
        self.selected = pad;
        self.shared.audition(pad);
    }

    fn controls(&mut self, ui: &mut Ui) {
        let secs = self.secs;
        let ms = 1000.0 * secs;
        // Shown a frame late, which at 60 fps nobody sees.
        let bpm = self.preset.bpm(secs);
        let mut play = None;
        let selected = &mut self.selected;
        let p = &mut self.preset;

        let ms_value = |ui: &mut Ui, v: &mut f32| {
            let mut at = *v * ms;
            ui.add(
                egui::DragValue::new(&mut at)
                    .range(0.0..=ms)
                    .speed(1.0)
                    .suffix(" ms"),
            );
            *v = at / ms;
        };

        egui::Grid::new("controls")
            .num_columns(2)
            .spacing([24.0, 8.0])
            .show(ui, |ui| {
                let b = &mut p.bounds;
                ui.label("Slices");
                ui.horizontal(|ui| {
                    for pad in 0..PADS {
                        let text = egui::RichText::new(format!("{} {}", pad + 1, FINGERS[pad]))
                            .color(PAD_COLORS[pad]);
                        if ui.selectable_label(*selected == pad, text).clicked() {
                            *selected = pad;
                        }
                    }
                    if ui
                        .button("▶")
                        .on_hover_text("Play the selected slice. Keys 1–4 (or space) play like fingers: hold to loop")
                        .clicked()
                    {
                        play = Some(*selected);
                    }
                    ui.separator();
                    let pad = &mut b.pads[*selected];
                    ms_value(ui, &mut pad.start);
                    ui.label("to");
                    ms_value(ui, &mut pad.end);
                    ui.weak(format!("{:.0} ms", pad.len() * ms));
                    ui.separator();
                    if ui
                        .button("Lay out on grid")
                        .on_hover_text(
                            "Put the four slices side by side from the start of the view",
                        )
                        .clicked()
                    {
                        *b = b.spread();
                    }
                });
                ui.end_row();

                ui.label("View");
                ui.horizontal(|ui| {
                    ms_value(ui, &mut b.start);
                    ui.label("to");
                    ms_value(ui, &mut b.end);
                    ui.weak(format!("{:.0} ms", (b.end - b.start) * ms));
                    if ui.button("Whole sample").clicked() {
                        (b.start, b.end) = (0.0, 1.0);
                    }
                });
                ui.end_row();

                ui.label("Grid");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::DragValue::new(&mut b.slices)
                            .range(0..=64)
                            .suffix(" lines"),
                    )
                    .on_hover_text("Grid lines across the view. 0 turns the grid off.");
                    ui.add_enabled(b.slices > 0, egui::Checkbox::new(&mut b.snap, "snap"))
                        .on_hover_text(
                            "Slice edges snap to the grid (hold shift to place freely), \
                             and pinching halves the slice rather than sweeping it",
                        );
                    ui.separator();
                    ui.add(
                        egui::DragValue::new(&mut p.beats)
                            .range(0.25..=256.0)
                            .speed(0.05)
                            .suffix(" beats"),
                    )
                    .on_hover_text("How many beats the view spans, which sets its tempo");
                    if ui.small_button("÷2").clicked() {
                        p.beats = (p.beats / 2.0).max(0.25);
                    }
                    if ui.small_button("×2").clicked() {
                        p.beats = (p.beats * 2.0).min(256.0);
                    }
                    ui.strong(format!("{bpm:.1} BPM"));
                    ui.checkbox(&mut p.follow_clock, "clock follows")
                        .on_hover_text(
                            "Retune the grid clock so taps quantize to the sample's own tempo",
                        );
                });
                ui.end_row();

                let b = &mut p.bounds;
                ui.label("Pinch");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Slider::new(&mut b.min_len_ms, 2.0..=1000.0)
                            .logarithmic(true)
                            .suffix(" ms")
                            .text("shortest"),
                    );
                    let full = b.pads[*selected].len() * ms;
                    if b.snap && full > b.min_len_ms {
                        let halvings = (full / b.min_len_ms).log2().floor();
                        ui.weak(format!(
                            "open plays the whole slice, pinched cuts it to {} of it",
                            fraction(halvings.exp2().recip())
                        ));
                    } else {
                        ui.weak("open plays the whole slice");
                    }
                });
                ui.end_row();
            });

        if let Some(pad) = play {
            self.audition(pad);
        }
    }

    /// The whole sample, with the view shaded. Drag an edge to move it, or
    /// sweep anywhere else for a new one; double-click for the whole sample.
    fn overview(&mut self, ui: &mut Ui, height: f32) {
        let (response, painter) =
            ui.allocate_painter(vec2(ui.available_width(), height), Sense::click_and_drag());
        let view = View::new(response.rect, 0.0, 1.0);
        let b = self.preset.bounds;

        painter.rect_filled(view.rect, 2.0, Color32::from_gray(18));
        let (x0, x1) = (view.x(b.start), view.x(b.end));
        let shaded = Rect::from_x_y_ranges(x0..=x1, view.rect.y_range());
        painter.rect_filled(shaded, 0.0, Color32::from_white_alpha(16));
        self.peaks.draw(&painter, &view, WAVE);
        // Where the slices are, as ticks along the bottom.
        for (pad, slice) in b.pads.iter().enumerate() {
            let r = Rect::from_x_y_ranges(
                view.x(slice.start)..=view.x(slice.end).max(view.x(slice.start) + 2.0),
                view.rect.bottom() - 5.0..=view.rect.bottom(),
            );
            painter.rect_filled(r, 0.0, PAD_COLORS[pad]);
        }
        for x in [x0, x1] {
            painter.vline(
                x,
                view.rect.y_range(),
                Stroke::new(2.0, Color32::from_rgb(0xff, 0xd0, 0x40)),
            );
        }
        self.draw_playhead(&painter, &view);

        self.edit_view(&response, &view);
    }

    fn edit_view(&mut self, response: &Response, view: &View) {
        let b = &mut self.preset.bounds;
        if let Some(at) = response.hover_pos() {
            let near = |f: f32| (view.x(f) - at.x).abs() < GRAB;
            if near(b.start) || near(b.end) {
                response
                    .ctx
                    .set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
            }
        }
        if response.double_clicked() {
            (b.start, b.end) = (0.0, 1.0);
            return;
        }
        let Some(at) = response.interact_pointer_pos() else {
            return;
        };
        let f = view.frac(at.x);
        if response.drag_started() {
            let (ds, de) = ((view.x(b.start) - at.x).abs(), (view.x(b.end) - at.x).abs());
            self.drag = Some(if ds.min(de) >= GRAB {
                Drag::NewView(f)
            } else if ds <= de {
                Drag::ViewStart
            } else {
                Drag::ViewEnd
            });
        }
        match self.drag {
            Some(Drag::ViewStart) => b.start = f.min(b.end),
            Some(Drag::ViewEnd) => b.end = f.max(b.start),
            Some(Drag::NewView(from)) if response.dragged() => {
                (b.start, b.end) = (from.min(f), from.max(f))
            }
            _ => {}
        }
        if response.drag_stopped() {
            self.drag = None;
        }
    }

    /// The view up close: a lane per finger with its slice, the grid, and
    /// the slice that is sounding.
    ///
    /// In a lane, drag an edge to resize, the middle to move, or empty space
    /// for a new slice; click to play it. On the waveform the same goes for
    /// the selected slice. Edges snap to the grid unless shift is held.
    fn closeup(&mut self, ui: &mut Ui) {
        let size = ui.available_size();
        let (response, painter) = ui.allocate_painter(size, Sense::click_and_drag());
        let b = self.preset.bounds;
        let view = View::new(response.rect, b.start, b.end);
        let lanes_bottom = view.rect.top() + LANE * PADS as f32;
        let wave = Rect::from_x_y_ranges(view.rect.x_range(), lanes_bottom..=view.rect.bottom());
        painter.rect_filled(view.rect, 2.0, Color32::from_gray(12));

        // Grid lines, with beats drawn brighter and numbered.
        let width = b.end - b.start;
        if b.slices > 0 {
            for i in 0..=b.slices {
                let f = b.start + width * i as f32 / b.slices as f32;
                painter.vline(
                    view.x(f),
                    view.rect.y_range(),
                    Stroke::new(1.0, Color32::from_gray(40)),
                );
            }
        }
        let beats = self.preset.beats;
        if (1.0..=64.0).contains(&beats) {
            for i in 0..beats.ceil() as u32 {
                let f = b.start + width * i as f32 / beats;
                painter.vline(
                    view.x(f),
                    view.rect.y_range(),
                    Stroke::new(1.0, Color32::from_gray(75)),
                );
                painter.text(
                    pos2(view.x(f) + 3.0, view.rect.bottom() - 2.0),
                    Align2::LEFT_BOTTOM,
                    (i + 1).to_string(),
                    FontId::monospace(10.0),
                    Color32::from_gray(90),
                );
            }
        }

        let head = self.shared.playhead();
        let sounding = head.playing.map(|v| v.pad);

        // The selected slice, shaded behind the waveform.
        let sel = b.pads[self.selected];
        let sel_rect = view.span(sel.start, sel.end, wave.y_range());
        painter.rect_filled(
            sel_rect,
            0.0,
            PAD_COLORS[self.selected].gamma_multiply(0.10),
        );
        // How much of the sounding slice plays, after pinching.
        if let Some(v) = head.playing {
            let r = view.span(v.start, v.start + head.len, wave.y_range());
            painter.rect_filled(r, 0.0, PAD_COLORS[v.pad].gamma_multiply(0.15));
            painter.rect_stroke(
                r,
                0.0,
                Stroke::new(1.0, PAD_COLORS[v.pad]),
                StrokeKind::Inside,
            );
        }
        self.peaks
            .draw(&painter, &View { rect: wave, ..view }, WAVE);

        for (pad, slice) in b.pads.iter().enumerate() {
            let lane = lane_rect(&view, pad);
            let color = PAD_COLORS[pad];
            if pad == self.selected {
                painter.rect_filled(lane, 0.0, Color32::from_white_alpha(6));
            }
            let bar = view
                .span(slice.start, slice.end, lane.y_range())
                .shrink2(vec2(0.0, 2.0));
            let lit = if sounding == Some(pad) { 1.0 } else { 0.55 };
            painter.rect_filled(bar, 3.0, color.gamma_multiply(lit));
            if pad == self.selected {
                painter.rect_stroke(
                    bar,
                    3.0,
                    Stroke::new(1.5, Color32::WHITE),
                    StrokeKind::Outside,
                );
            }
            painter.text(
                pos2(bar.left().max(lane.left()) + 4.0, lane.center().y),
                Align2::LEFT_CENTER,
                format!("{} {}", pad + 1, FINGERS[pad]),
                FontId::proportional(12.0),
                Color32::BLACK,
            );
        }
        self.draw_playhead(&painter, &view);

        let shift = ui.input(|i| i.modifiers.shift);
        self.edit_pads(&response, &view, lanes_bottom, shift);
    }

    fn edit_pads(&mut self, response: &Response, view: &View, lanes_bottom: f32, free: bool) {
        let Some(at) = response.interact_pointer_pos().or(response.hover_pos()) else {
            return;
        };
        // The lane under the pointer, or the selected pad on the waveform.
        let pad = if at.y < lanes_bottom {
            (((at.y - view.rect.top()) / LANE) as usize).min(PADS - 1)
        } else {
            self.selected
        };
        let slice = self.preset.bounds.pads[pad];
        let near = |f: f32| (view.x(f) - at.x).abs() < GRAB;
        let grab = if near(slice.start) {
            PadDrag::Start
        } else if near(slice.end) {
            PadDrag::End
        } else if (view.x(slice.start)..=view.x(slice.end)).contains(&at.x) {
            PadDrag::Move(view.frac(at.x) - slice.start)
        } else {
            PadDrag::New(view.frac(at.x))
        };
        if response.hovered() && self.drag.is_none() {
            response.ctx.set_cursor_icon(match grab {
                PadDrag::Start | PadDrag::End => egui::CursorIcon::ResizeHorizontal,
                PadDrag::Move(_) => egui::CursorIcon::Grab,
                PadDrag::New(_) => egui::CursorIcon::Text,
            });
        }

        if response.clicked() {
            self.audition(pad);
            return;
        }
        if response.drag_started() {
            self.selected = pad;
            self.drag = Some(Drag::Pad(pad, grab));
        }
        let b = &mut self.preset.bounds;
        let snap = |f: f32| {
            if free {
                f.clamp(0.0, 1.0)
            } else {
                b.snap_point(f)
            }
        };
        let f = view.frac(at.x);
        if let Some(Drag::Pad(pad, how)) = self.drag {
            let s = b.pads[pad];
            let next = match how {
                PadDrag::Start => Slice {
                    start: snap(f).min(s.end),
                    ..s
                },
                PadDrag::End => Slice {
                    end: snap(f).max(s.start),
                    ..s
                },
                PadDrag::Move(grabbed) => {
                    let start = snap(f - grabbed).clamp(0.0, 1.0 - s.len());
                    Slice {
                        start,
                        end: start + s.len(),
                    }
                }
                PadDrag::New(from) => {
                    let (a, z) = (snap(from), snap(f));
                    Slice {
                        start: a.min(z),
                        end: a.max(z),
                    }
                }
            };
            b.pads[pad] = next;
        }
        if response.drag_stopped() {
            self.drag = None;
        }
    }

    fn draw_playhead(&self, painter: &Painter, view: &View) {
        if let Some(v) = self.shared.playhead().playing {
            let x = view.x(v.at);
            if view.rect.x_range().contains(x) {
                painter.vline(x, view.rect.y_range(), Stroke::new(1.5, PLAYHEAD));
            }
        }
    }
}

fn lane_rect(view: &View, pad: usize) -> Rect {
    let top = view.rect.top() + LANE * pad as f32;
    Rect::from_x_y_ranges(view.rect.x_range(), top..=top + LANE)
}

impl eframe::App for Viewer {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, Key::S)) {
            self.save();
        }
        // Keys 1–4 (and space, for the selected pad) are fingers: held, the
        // slice loops, so the right hand can play with it.
        let mut held = 0;
        if !ui.ctx().egui_wants_keyboard_input() {
            let keys = [Key::Num1, Key::Num2, Key::Num3, Key::Num4];
            for (pad, key) in keys.into_iter().enumerate() {
                let (pressed, down) = ui.input(|i| (i.key_pressed(key), i.key_down(key)));
                if pressed {
                    self.selected = pad;
                }
                if down {
                    held |= 1 << pad;
                } else if pressed {
                    // Down and up within one frame: still a tap.
                    self.shared.audition(pad);
                }
            }
            if ui.input(|i| i.key_down(Key::Space)) {
                held |= 1 << self.selected;
            }
        }
        self.shared.set_held(held);

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(4.0);
            self.header(ui);
            ui.add_space(2.0);
        });
        egui::Panel::bottom("controls").show(ui, |ui| {
            ui.add_space(6.0);
            self.controls(ui);
            ui.add_space(6.0);
        });
        egui::CentralPanel::default().show(ui, |ui| {
            self.overview(ui, 70.0);
            ui.add_space(6.0);
            self.closeup(ui);
        });

        self.sync();
        // The playhead moves whether or not the mouse does.
        ui.ctx().request_repaint();
    }
}

/// `1/4`, `1/2`, `1`, `2`, ... for a power of two.
fn fraction(steps: f32) -> String {
    if steps >= 1.0 {
        format!("{steps}")
    } else {
        format!("1/{}", (1.0 / steps).round())
    }
}

/// Maps a span of the sample, in fractions, onto a rect.
#[derive(Debug, Clone, Copy)]
struct View {
    rect: Rect,
    from: f32,
    to: f32,
}

impl View {
    fn new(rect: Rect, from: f32, to: f32) -> Self {
        Self {
            rect,
            from,
            to: if to > from { to } else { from + f32::EPSILON },
        }
    }

    fn x(&self, f: f32) -> f32 {
        self.rect.left() + (f - self.from) / (self.to - self.from) * self.rect.width()
    }

    fn frac(&self, x: f32) -> f32 {
        (self.from + (x - self.rect.left()) / self.rect.width() * (self.to - self.from))
            .clamp(0.0, 1.0)
    }

    /// `from..to` across `y`, clipped to the view and at least a point wide.
    fn span(&self, from: f32, to: f32, y: egui::Rangef) -> Rect {
        let (l, r) = (self.rect.left(), self.rect.right());
        let x0 = self.x(from).clamp(l, r);
        let x1 = self.x(to).clamp(l, r).max((x0 + 1.0).min(r));
        Rect::from_x_y_ranges(x0..=x1, y)
    }
}

/// Min/max of the sample over fixed buckets, so drawing any span is quick.
struct Peaks {
    min: Vec<f32>,
    max: Vec<f32>,
    /// Scales the loudest peak to fill the view.
    gain: f32,
}

impl Peaks {
    const BUCKET: usize = 32;

    fn new(mono: &[f32]) -> Self {
        let (min, max): (Vec<f32>, Vec<f32>) = mono
            .chunks(Self::BUCKET)
            .map(|c| {
                c.iter()
                    .fold((0.0f32, 0.0f32), |(lo, hi), &s| (lo.min(s), hi.max(s)))
            })
            .unzip();
        let loudest = min.iter().chain(&max).fold(0.0f32, |m, s| m.max(s.abs()));
        Self {
            min,
            max,
            gain: if loudest > 0.0 { 0.95 / loudest } else { 1.0 },
        }
    }

    fn draw(&self, painter: &Painter, view: &View, color: Color32) {
        let n = self.min.len();
        if n == 0 {
            return;
        }
        let r = view.rect;
        let mid = r.center().y;
        let half = r.height() * 0.5 * self.gain;
        let bucket = |f: f32| ((f * n as f32) as usize).min(n - 1);
        let columns = r.width().max(1.0) as usize;
        for col in 0..columns {
            let x = r.left() + col as f32 + 0.5;
            let a = view.frac(r.left() + col as f32);
            let b = view.frac(r.left() + col as f32 + 1.0);
            let (lo, hi) = (bucket(a)..=bucket(b).max(bucket(a)))
                .fold((0.0f32, 0.0f32), |(lo, hi), i| {
                    (lo.min(self.min[i]), hi.max(self.max[i]))
                });
            painter.line_segment(
                [
                    Pos2::new(x, mid - hi * half),
                    Pos2::new(x, mid - lo * half + 1.0),
                ],
                Stroke::new(1.0, color),
            );
        }
    }
}

/// Bounds from the slicer's handle, then the saved file next to the sample if
/// there is one. Returns the preset and whether it was loaded from disk.
pub fn load_preset(
    sample: &std::path::Path,
    defaults: Bounds,
    secs: f32,
) -> Result<(Preset, bool), String> {
    let guess = Preset::guess(defaults, secs);
    let path = Preset::path_for(sample);
    match std::fs::read_to_string(&path) {
        Ok(text) => guess
            .from_text(&text)
            .map(|p| (p, true))
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((guess, false)),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}
