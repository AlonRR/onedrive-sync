// Dependency-free icon rasteriser shared by the runtime (tray + window icon) and
// build.rs (the embedded .exe icon), so all three are the exact same artwork.
// Draws a white two-way "sync" glyph — an up arrow beside a down arrow — on a
// rounded tile of the given colour. Supersampled for clean edges at small sizes.
//
// Kept std-only so build.rs can `include!` this file directly.

/// The healthy/brand tile colour (matches the tray's "ok" green).
pub const BRAND: [u8; 3] = [16, 124, 16];

/// RGBA8 pixels (row-major, top-left origin) for a `size`x`size` icon, white glyph
/// on a rounded tile of colour `bg`; transparent outside the tile.
pub fn rgba(size: u32, bg: [u8; 3]) -> Vec<u8> {
    const SS: u32 = 4; // supersample factor
    let n = size * SS;
    let mut acc = vec![[0u32; 4]; (size * size) as usize];
    for iy in 0..n {
        for ix in 0..n {
            let u = (ix as f32 + 0.5) / n as f32;
            let v = (iy as f32 + 0.5) / n as f32;
            let (r, g, b, a) = sample(u, v, bg);
            let p = &mut acc[((iy / SS) * size + (ix / SS)) as usize];
            p[0] += r as u32;
            p[1] += g as u32;
            p[2] += b as u32;
            p[3] += a as u32;
        }
    }
    let div = (SS * SS) as u32;
    let mut out = vec![0u8; (size * size * 4) as usize];
    for i in 0..(size * size) as usize {
        out[i * 4] = (acc[i][0] / div) as u8;
        out[i * 4 + 1] = (acc[i][1] / div) as u8;
        out[i * 4 + 2] = (acc[i][2] / div) as u8;
        out[i * 4 + 3] = (acc[i][3] / div) as u8;
    }
    out
}

fn sample(u: f32, v: f32, bg: [u8; 3]) -> (u8, u8, u8, u8) {
    if !in_rounded_square(u, v, 0.18) {
        return (0, 0, 0, 0);
    }
    if in_glyph(u, v) {
        (255, 255, 255, 255)
    } else {
        (bg[0], bg[1], bg[2], 255)
    }
}

fn in_rounded_square(u: f32, v: f32, r: f32) -> bool {
    let (lo, hi) = (0.04f32, 0.96f32);
    if u < lo || u > hi || v < lo || v > hi {
        return false;
    }
    let cx = u.clamp(lo + r, hi - r);
    let cy = v.clamp(lo + r, hi - r);
    let (dx, dy) = (u - cx, v - cy);
    dx * dx + dy * dy <= r * r
}

/// Left arrow points up, right arrow points down — the universal two-way glyph.
fn in_glyph(u: f32, v: f32) -> bool {
    let sw = 0.05; // shaft half-width
    let hw = 0.115; // arrowhead half-width

    let lx = 0.365;
    let up_shaft = (u - lx).abs() < sw && (0.40..0.80).contains(&v);
    let up_head = (0.17..=0.42).contains(&v) && (u - lx).abs() <= hw * ((v - 0.17) / 0.25);

    let rx = 0.635;
    let dn_shaft = (u - rx).abs() < sw && (0.20..0.60).contains(&v);
    let dn_head = (0.58..=0.83).contains(&v) && (u - rx).abs() <= hw * ((0.83 - v) / 0.25);

    up_shaft || up_head || dn_shaft || dn_head
}
