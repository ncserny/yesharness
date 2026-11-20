//! Small vector icons, drawn with the painter so they look crisp and match the theme (no emoji).

use eframe::egui::{pos2, vec2, Color32, Painter, Pos2, Rect, Shape, Stroke};
use std::f32::consts::PI;

#[derive(Clone, Copy, PartialEq)]
pub enum Icon {
    Mic,
    Speaker,
    SpeakerOff,
    Keyboard,
    Plus,
    Refresh,
    Ellipsis,
    Stop,
    Send,
    Gear,
    People,
    Play,
    Close,
    Back,
}

fn arc(c: Pos2, r: f32, a0: f32, a1: f32) -> Vec<Pos2> {
    let n = 18;
    (0..=n).map(|i| {
        let t = a0 + (a1 - a0) * i as f32 / n as f32;
        c + vec2(r * t.cos(), r * t.sin())
    }).collect()
}

/// Paint `icon` inside `rect` (roughly square).
pub fn paint(p: &Painter, icon: Icon, rect: Rect, color: Color32) {
    let c = rect.center();
    let s = rect.width().min(rect.height()) / 16.0; // design grid: 16 units
    let at = |x: f32, y: f32| c + vec2(x * s, y * s);
    let st = Stroke::new(1.5 * s.max(0.8), color);
    match icon {
        Icon::Mic => {
            p.rect_filled(Rect::from_center_size(at(0.0, -2.5), vec2(5.5 * s, 9.5 * s)), 2.75 * s, color);
            p.add(Shape::line(arc(at(0.0, -1.5), 5.0 * s, 0.0, PI), st));
            p.line_segment([at(0.0, 3.5), at(0.0, 6.5)], st);
            p.line_segment([at(-2.5, 6.5), at(2.5, 6.5)], st);
        }
        Icon::Speaker | Icon::SpeakerOff => {
            let body = vec![at(-6.5, -2.0), at(-3.5, -2.0), at(0.5, -5.5), at(0.5, 5.5), at(-3.5, 2.0), at(-6.5, 2.0)];
            p.add(Shape::convex_polygon(body, color, Stroke::NONE));
            if icon == Icon::Speaker {
                p.add(Shape::line(arc(at(1.0, 0.0), 3.2 * s, -0.9, 0.9), st));
                p.add(Shape::line(arc(at(1.0, 0.0), 6.0 * s, -0.9, 0.9), st));
            } else {
                p.line_segment([at(3.0, -2.5), at(7.0, 2.5)], st);
                p.line_segment([at(3.0, 2.5), at(7.0, -2.5)], st);
            }
        }
        Icon::Keyboard => {
            p.rect_stroke(Rect::from_center_size(c, vec2(15.0 * s, 10.0 * s)), 2.0 * s, st, eframe::egui::StrokeKind::Middle);
            for (y, xs) in [(-2.2, [-4.5, -1.5, 1.5, 4.5]), (0.6, [-4.5, -1.5, 1.5, 4.5])] {
                for x in xs {
                    p.circle_filled(at(x, y), 0.9 * s, color);
                }
            }
            p.line_segment([at(-3.0, 3.2), at(3.0, 3.2)], st);
        }
        Icon::Plus => {
            p.line_segment([at(-5.5, 0.0), at(5.5, 0.0)], st);
            p.line_segment([at(0.0, -5.5), at(0.0, 5.5)], st);
        }
        Icon::Refresh => {
            p.add(Shape::line(arc(c, 5.5 * s, -PI * 0.35, PI * 1.45), st));
            let tip = c + vec2(5.5 * s * (-PI * 0.35).cos(), 5.5 * s * (-PI * 0.35).sin());
            p.add(Shape::convex_polygon(vec![tip + vec2(-3.4 * s, -1.8 * s), tip + vec2(1.2 * s, 2.6 * s), tip + vec2(1.4 * s, -2.6 * s)], color, Stroke::NONE));
        }
        Icon::Ellipsis => {
            for x in [-5.0, 0.0, 5.0] {
                p.circle_filled(at(x, 0.0), 1.5 * s, color);
            }
        }
        Icon::Gear => {
            for i in 0..8 {
                let a = i as f32 * PI / 4.0;
                p.line_segment([c + vec2(a.cos(), a.sin()) * 4.6 * s, c + vec2(a.cos(), a.sin()) * 7.2 * s], Stroke::new(2.2 * s.max(0.8), color));
            }
            p.circle_stroke(c, 4.6 * s, st);
            p.circle_stroke(c, 1.8 * s, st);
        }
        Icon::People => {
            p.circle_stroke(at(0.0, -3.0), 3.0 * s, st);
            p.add(Shape::line(arc(at(0.0, 7.5), 6.3 * s, PI * 1.12, PI * 1.88), st));
        }
        Icon::Stop => {
            p.rect_filled(Rect::from_center_size(c, vec2(9.0 * s, 9.0 * s)), 2.0 * s, color);
        }
        Icon::Send => {
            p.circle_filled(c, 8.0 * s, color);
            let w = Stroke::new(1.8 * s.max(0.8), Color32::WHITE);
            p.line_segment([at(0.0, 4.0), at(0.0, -4.0)], w);
            p.line_segment([at(-3.5, -0.5), at(0.0, -4.0)], w);
            p.line_segment([at(3.5, -0.5), at(0.0, -4.0)], w);
        }
        Icon::Close => {
            p.line_segment([at(-4.5, -4.5), at(4.5, 4.5)], st);
            p.line_segment([at(-4.5, 4.5), at(4.5, -4.5)], st);
        }
        Icon::Back => {
            p.add(Shape::line(vec![at(2.5, -5.5), at(-3.0, 0.0), at(2.5, 5.5)], st));
        }
        Icon::Play => {
            p.add(Shape::convex_polygon(vec![at(-3.5, -5.0), at(5.0, 0.0), at(-3.5, 5.0)], color, Stroke::NONE));
        }
    }
}

/// A round avatar with an initial.
pub fn avatar(p: &Painter, center: Pos2, radius: f32, color: Color32, initial: char) {
    p.circle_filled(center, radius, color);
    p.text(
        center + vec2(0.0, radius * 0.04),
        eframe::egui::Align2::CENTER_CENTER,
        initial.to_ascii_uppercase(),
        eframe::egui::FontId::proportional(radius * 1.05),
        Color32::WHITE,
    );
}

/// Three dots that bounce, for "working…".
pub fn typing_dots(p: &Painter, left: Pos2, color: Color32, time: f64) {
    for i in 0..3 {
        let phase = (time as f32 * 4.0 - i as f32 * 0.6).sin() * 0.5 + 0.5;
        p.circle_filled(pos2(left.x + i as f32 * 8.0, left.y - phase * 2.5), 2.6, color.gamma_multiply(0.45 + 0.55 * phase));
    }
}

