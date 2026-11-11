//! Native GPUI port of the approved Thinking Orbs `composing` / 20px preset.
//! Geometry adapted from Jakub Antalik's MIT-licensed thinking-orbs:
//! https://github.com/Jakubantalik/Libraries.dev/tree/387b049ca8fe97b5d72999b5bf61a19ab1ccff81/packages/thinking-orbs
//! See assets/licenses/Thinking-Orbs-MIT.txt for the upstream notice.

use std::{f64::consts::TAU, sync::OnceLock, time::Instant};

use gpui::{canvas, prelude::*, px, Bounds, Hsla, IntoElement};

const SIZE: f64 = 20.0;
const PRESET_SPEED: f64 = 3.12;
const SELECTED_SPEED: f64 = 2.05;
const GHOSTS: usize = 8;
// Upstream resolved preset: 2 base lanes × bandMul 4.94, rounded to 10.
const LANES: usize = 10;
const SEGMENTS: usize = 20;

#[derive(Clone, Copy, Debug)]
struct Dot {
    x: f64,
    y: f64,
    z: f64,
    radius: f64,
    white: f64,
    alpha: f64,
}

// Time is the upstream engine clock; the caller applies the preset speed.
fn frame(time: f64) -> Vec<Dot> {
    // Composing has spin=0: preserve its fixed tilted ribbon orientation.
    let (st, ct) = 0.3_f64.sin_cos();
    let (band_sin, band_cos) = 0.55_f64.sin_cos();
    let radius = SIZE * 0.5 * 0.78;
    let radius_scale = (SIZE / 300.0).powf(0.6);
    let project = |x: f64, y: f64, z: f64| {
        (
            SIZE / 2.0 + x,
            SIZE / 2.0 - (y * ct - z * st),
            y * st + z * ct,
        )
    };
    let mut dots = Vec::with_capacity(GHOSTS + LANES * SEGMENTS);
    // The faint spherical scaffold uses the same Fibonacci lattice as upstream.
    let golden_angle = std::f64::consts::PI * (3.0 - 5.0_f64.sqrt());
    for index in 0..GHOSTS {
        let y = 1.0 - 2.0 * (index as f64 + 0.5) / GHOSTS as f64;
        let radial = (1.0 - y * y).sqrt();
        let angle = index as f64 * golden_angle;
        let (x, y, z) = project(
            radial * angle.cos() * radius,
            y * radius,
            radial * angle.sin() * radius,
        );
        let depth = (z / radius + 1.0) / 2.0;
        dots.push(Dot {
            x,
            y,
            z,
            radius: (0.8 * radius_scale).max(0.3),
            white: 0.78,
            alpha: 0.1 + 0.22 * depth,
        });
    }
    let half_lanes = (LANES - 1) as f64 / 2.0;
    for lane in 0..LANES {
        let lane_offset = (lane as f64 - half_lanes) * 0.075;
        let edge = (lane as f64 - half_lanes).abs() / half_lanes;
        for segment in 0..SEGMENTS {
            let angle = segment as f64 / SEGMENTS as f64 * TAU;
            let wobble = 0.16 * (angle * 3.0 - time * 1.7 + lane as f64 * 0.22).sin()
                + 0.07 * (angle * 5.0 + time * 1.1).sin();
            let offset = lane_offset + wobble;
            let x = angle.cos();
            let y = band_cos * angle.sin() - band_sin * offset;
            let z = band_sin * angle.sin() + band_cos * offset;
            let length = (x * x + y * y + z * z).sqrt();
            let (x, y, z) = project(
                x / length * radius,
                y / length * radius,
                z / length * radius,
            );
            let depth = (z / radius + 1.0) / 2.0;
            dots.push(Dot {
                x,
                y,
                z,
                radius: ((1.1803 + 1.8241 * depth) * (1.0 - 0.25 * edge) * radius_scale).max(0.3),
                white: 0.52 - 0.44 * depth + 0.18 * edge,
                alpha: 0.4 + 0.6 * depth,
            });
        }
    }
    dots.sort_by(|a, b| a.z.total_cmp(&b.z));
    dots
}

fn animation_time(elapsed_seconds: f64) -> f64 {
    elapsed_seconds * PRESET_SPEED * SELECTED_SPEED
}

// Match upstream's quantized monochrome ink, rather than tinting the dots
// with the app foreground. This preserves the approved light/dark appearance.
fn ink(dot: &Dot, dark: bool) -> Hsla {
    let white = dot.white.clamp(0.0, 1.0);
    let channel = ((if dark { 1.0 - white } else { white }) * 255.0).round() as u32;
    Hsla::from(gpui::rgb((channel << 16) | (channel << 8) | channel)).opacity(dot.alpha as f32)
}

/// The approved composing/20px/2.05× orb. Only mounted while work runs.
/// Reduced motion and inactive windows paint a stable representative frame.
pub fn thinking_orb(dark: bool, reduced_motion: bool) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            static START: OnceLock<Instant> = OnceLock::new();
            let animate = window.is_window_active() && !reduced_motion;
            let time = if animate {
                animation_time(START.get_or_init(Instant::now).elapsed().as_secs_f64())
            } else {
                0.6
            };
            for dot in frame(time) {
                let radius = px(dot.radius as f32);
                let dot_bounds = Bounds {
                    origin: bounds.origin + gpui::point(px(dot.x as f32), px(dot.y as f32))
                        - gpui::point(radius, radius),
                    size: gpui::size(radius * 2.0, radius * 2.0),
                };
                window.paint_quad(gpui::quad(
                    dot_bounds,
                    gpui::Corners::all(radius),
                    ink(&dot, dark),
                    gpui::Edges::all(px(0.0)),
                    gpui::transparent_black(),
                    gpui::BorderStyle::Solid,
                ));
            }
            if animate {
                window.request_animation_frame();
            }
        },
    )
    .flex_none()
    .w(px(SIZE as f32))
    .h(px(SIZE as f32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_geometry_matches_upstream_reference_frames() {
        let cases: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/orb-composing-20.json"))
                .unwrap();
        for case in cases.as_array().unwrap() {
            let dots = frame(case["t"].as_f64().unwrap());
            let expected = case["dots"].as_array().unwrap();
            assert_eq!(dots.len() * 6, expected.len());
            for (dot, reference) in dots.iter().zip(expected.chunks_exact(6)) {
                for (actual, expected) in [dot.x, dot.y, dot.z, dot.radius, dot.white, dot.alpha]
                    .into_iter()
                    .zip(reference)
                {
                    assert!(
                        (actual - expected.as_f64().unwrap()).abs() < 0.0001,
                        "{}: {actual} differs from {expected}",
                        case["key"]
                    );
                }
            }
        }
    }

    #[test]
    fn animation_stays_inside_its_tab_slot() {
        for sample in 0..1000 {
            let dots = frame(sample as f64 * 0.1);
            for dot in dots {
                assert!(dot.x - dot.radius >= 0.0 && dot.x + dot.radius <= SIZE);
                assert!(dot.y - dot.radius >= 0.0 && dot.y + dot.radius <= SIZE);
                assert!((0.0..=1.0).contains(&dot.alpha));
            }
        }
        assert!(frame(0.6)
            .iter()
            .zip(frame(1.7))
            .any(|(a, b)| (a.x - b.x).abs() > 0.001));
    }

    #[test]
    fn selected_speed_matches_gallery_multiplier() {
        // The gallery multiplies the composing preset's 3.12 clock by 2.05.
        assert!((animation_time(1.0) - 6.396).abs() < 1e-12);
        assert!((animation_time(10.0) - 63.96).abs() < 1e-12);
    }

    #[test]
    fn ink_matches_upstream_light_and_dark_grayscale() {
        let dot = Dot {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            radius: 1.0,
            white: 0.52,
            alpha: 0.7,
        };
        let light = ink(&dot, false);
        let dark = ink(&dot, true);
        assert!((light.l - 133.0 / 255.0).abs() < 1e-6);
        assert!((dark.l - 122.0 / 255.0).abs() < 1e-6);
        assert_eq!(light.a, 0.7);
        assert_eq!(dark.a, 0.7);
    }
}
