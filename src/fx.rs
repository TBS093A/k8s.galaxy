//! Tiny effects toolkit: colors, easing, deterministic noise.
//! Seed of a future reusable `tui-fx` crate.

use std::f64::consts::TAU;

use ratatui::style::Color;

pub type Rgb = (u8, u8, u8);

pub const BG: Rgb = (4, 7, 12);
pub const CYAN: Rgb = (0, 229, 255);
pub const MAGENTA: Rgb = (255, 0, 170);
pub const GREEN: Rgb = (0, 255, 140);
pub const AMBER: Rgb = (255, 184, 0);
pub const RED: Rgb = (255, 45, 85);
pub const STEEL: Rgb = (90, 120, 140);
pub const LAND: Rgb = (24, 62, 80);
pub const GRID: Rgb = (12, 24, 34);

pub fn c(rgb: Rgb) -> Color {
    Color::Rgb(rgb.0, rgb.1, rgb.2)
}

/// Linear blend between two colors, k clamped to 0..1.
pub fn mix(a: Rgb, b: Rgb, k: f64) -> Rgb {
    let k = k.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * k).round() as u8;
    (ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
}

/// Color faded toward the background — simulates glow falloff / transparency.
pub fn fade(rgb: Rgb, intensity: f64) -> Color {
    c(mix(BG, rgb, intensity))
}

/// Smooth 0..1..0 oscillation with given period (seconds).
pub fn pulse(t: f64, period: f64) -> f64 {
    0.5 - 0.5 * (t / period * TAU).cos()
}

/// FNV-1a — stable per-name seeds for positions and phases.
pub fn hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Map a hash to 0..1.
pub fn unit(h: u64) -> f64 {
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// xorshift64* PRNG, good enough for demo data and starfields.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn f64(&mut self) -> f64 {
        unit(self.next_u64())
    }

    pub fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[(self.next_u64() % items.len() as u64) as usize]
    }
}
