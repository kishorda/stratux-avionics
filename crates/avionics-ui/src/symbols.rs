//! Target symbol shapes, keyed off the GDL90 emitter category.
//!
//! Every symbol is built centred on the origin and pointing "up", so the caller rotates by the
//! screen angle of the target's track. Shapes are chosen to stay distinguishable at roughly 16 px
//! on a 7" panel — that rules out anything with interior detail.

use avionics_gfx::femtovg::Path;

/// The visually distinct symbol families we draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolShape {
    /// Powered fixed-wing, any weight class.
    Aircraft,
    Rotorcraft,
    Glider,
    /// Balloon or airship.
    LighterThanAir,
    /// Parachutist, hang glider, paraglider, ultralight.
    Light,
    Uav,
    /// Airport service or emergency vehicle on the surface.
    GroundVehicle,
    Obstacle,
    Unknown,
}

/// Map a GDL90 emitter category to a shape.
///
/// Categories are from DO-282 / the GDL90 interface spec, as forwarded by Stratux in
/// `Emitter_category`. Weight classes 1–6 all draw as `Aircraft`: the distinction matters for
/// wake turbulence, not for seeing traffic at a glance.
pub fn shape_for_category(category: u8) -> SymbolShape {
    match category {
        1..=6 => SymbolShape::Aircraft,
        7 => SymbolShape::Rotorcraft,
        9 => SymbolShape::Glider,
        10 => SymbolShape::LighterThanAir,
        11 | 12 => SymbolShape::Light,
        14 | 15 => SymbolShape::Uav,
        17 | 18 => SymbolShape::GroundVehicle,
        19..=21 => SymbolShape::Obstacle,
        // 0 is "no information", 8/13/16 are unassigned.
        _ => SymbolShape::Unknown,
    }
}

/// Build a symbol centred on the origin, pointing up, scaled so `size` is roughly its radius.
pub fn build(shape: SymbolShape, size: f32) -> Path {
    let mut path = Path::new();
    let s = size;

    match shape {
        SymbolShape::Aircraft => {
            // Chevron: non-convex, reads clearly as directional even when small.
            path.move_to(0.0, -s);
            path.line_to(s * 0.82, s * 0.80);
            path.line_to(0.0, s * 0.34);
            path.line_to(-s * 0.82, s * 0.80);
            path.close();
        }
        SymbolShape::Glider => {
            // Slenderer and longer than a powered aircraft.
            path.move_to(0.0, -s * 1.15);
            path.line_to(s * 0.50, s * 0.85);
            path.line_to(0.0, s * 0.45);
            path.line_to(-s * 0.50, s * 0.85);
            path.close();
        }
        SymbolShape::Rotorcraft => {
            // Body plus a rotor bar; the bar is what distinguishes it at a glance.
            path.circle(0.0, 0.0, s * 0.52);
            path.move_to(-s * 1.05, -s * 0.72);
            path.line_to(s * 1.05, s * 0.72);
            path.move_to(-s * 1.05, s * 0.72);
            path.line_to(s * 1.05, -s * 0.72);
        }
        SymbolShape::LighterThanAir => {
            path.circle(0.0, 0.0, s * 0.78);
        }
        SymbolShape::Light => {
            // Small diamond: clearly "something small and slow".
            path.move_to(0.0, -s * 0.72);
            path.line_to(s * 0.60, 0.0);
            path.line_to(0.0, s * 0.72);
            path.line_to(-s * 0.60, 0.0);
            path.close();
        }
        SymbolShape::Uav => {
            // Square with a directional notch on top.
            path.move_to(-s * 0.62, -s * 0.34);
            path.line_to(0.0, -s * 0.86);
            path.line_to(s * 0.62, -s * 0.34);
            path.line_to(s * 0.62, s * 0.62);
            path.line_to(-s * 0.62, s * 0.62);
            path.close();
        }
        SymbolShape::GroundVehicle => {
            path.rect(-s * 0.58, -s * 0.42, s * 1.16, s * 0.84);
        }
        SymbolShape::Obstacle => {
            path.move_to(0.0, -s * 0.95);
            path.line_to(s * 0.80, s * 0.70);
            path.line_to(-s * 0.80, s * 0.70);
            path.close();
        }
        SymbolShape::Unknown => {
            path.move_to(0.0, -s * 0.85);
            path.line_to(s * 0.85, 0.0);
            path.line_to(0.0, s * 0.85);
            path.line_to(-s * 0.85, 0.0);
            path.close();
        }
    }

    path
}

/// Shapes drawn as strokes rather than fills.
pub fn is_stroke_only(shape: SymbolShape) -> bool {
    matches!(shape, SymbolShape::Rotorcraft)
}

/// Own-ship symbol: a chevron, distinct from traffic only by colour and by always pointing up.
pub fn ownship(size: f32) -> Path {
    let mut path = Path::new();
    path.move_to(0.0, -size * 1.25);
    path.line_to(size * 0.88, size * 0.88);
    path.line_to(0.0, size * 0.42);
    path.line_to(-size * 0.88, size * 0.88);
    path.close();
    path
}

/// A short vertical bar from the symbol, showing where the target is heading.
pub fn heading_barb(size: f32, length: f32) -> Path {
    let mut path = Path::new();
    path.move_to(0.0, -size);
    path.line_to(0.0, -size - length);
    path
}
