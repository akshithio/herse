//! herse: colors authors chose for one background (a terminal's black, a
//! notebook's white) kept readable on the other.

use gpui::{Hsla, Rgba};

/// WCAG relative luminance.
pub fn luminance(color: Hsla) -> f32 {
    let c = Rgba::from(color);
    let channel = |v: f32| {
        if v <= 0.039_28 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
}

/// `color` as text on a dark or light page: its lightness moved, hue and
/// saturation kept, until it reads (about 4.5:1 on a near-black page, 3:1
/// on white). Colors that already read are returned unchanged.
pub fn readable_text_color(color: Hsla, dark: bool) -> Hsla {
    // Luminance bounds for the page colors Herse uses (#191817, #FFFFFF).
    const ON_DARK_MIN: f32 = 0.23;
    const ON_LIGHT_MAX: f32 = 0.30;
    let lum = luminance(color);
    let with_l = |l: f32| Hsla { l, ..color };
    if dark {
        if lum >= ON_DARK_MIN {
            return color;
        }
        let (mut lo, mut hi) = (color.l, 1.0_f32);
        for _ in 0..14 {
            let mid = (lo + hi) / 2.;
            if luminance(with_l(mid)) >= ON_DARK_MIN { hi = mid } else { lo = mid }
        }
        with_l(hi)
    } else {
        if lum <= ON_LIGHT_MAX {
            return color;
        }
        let (mut lo, mut hi) = (0.0_f32, color.l);
        for _ in 0..14 {
            let mid = (lo + hi) / 2.;
            if luminance(with_l(mid)) <= ON_LIGHT_MAX { lo = mid } else { hi = mid }
        }
        with_l(lo)
    }
}

/// Ink for text on an author's background (a `<mark>`): dark on light
/// fills, light on dark ones.
pub fn ink_on(background: Hsla) -> Hsla {
    if luminance(background) > 0.35 {
        gpui::hsla(30. / 360., 0.1, 0.1, 1.)
    } else {
        gpui::hsla(40. / 360., 0.12, 0.92, 1.)
    }
}
