//! CSS colors, colorscales and scheme lookup.

use crate::schemes;

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    /// 0..=1
    pub a: f32,
}

impl Rgba {
    pub const BLACK: Rgba = Rgba::rgb(0, 0, 0);
    pub const WHITE: Rgba = Rgba::rgb(255, 255, 255);
    pub const TRANSPARENT: Rgba = Rgba { r: 0, g: 0, b: 0, a: 0. };

    pub const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
        Rgba { r, g, b, a: 1. }
    }

    pub fn hex_rgb(v: u32) -> Rgba {
        Rgba::rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }

    /// Parses a CSS color: `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`, `rgb()`,
    /// `rgba()`, `hsl()`, `hsla()`, named colors and `transparent`.
    pub fn parse(s: &str) -> Option<Rgba> {
        let s = s.trim();
        if s.is_empty() || s.len() > 64 {
            return None;
        }
        if let Some(hex) = s.strip_prefix('#') {
            return parse_hex(hex);
        }
        let lower = s.to_ascii_lowercase();
        if let Some(open) = lower.find('(') {
            let name = lower[..open].trim();
            let inner = lower[open + 1..].trim_end().strip_suffix(')')?;
            let parts: Vec<&str> = inner
                .split(|c: char| c == ',' || c == '/' || c.is_whitespace())
                .filter(|p| !p.is_empty())
                .collect();
            if parts.len() < 3 || parts.len() > 4 {
                return None;
            }
            let alpha = match parts.get(3) {
                Some(p) => parse_unit(p, 1.)?,
                None => 1.,
            };
            return match name {
                "rgb" | "rgba" => {
                    let c = |p: &str| -> Option<u8> { Some((parse_unit(p, 255.)?).round().clamp(0., 255.) as u8) };
                    Some(Rgba {
                        r: c(parts[0])?,
                        g: c(parts[1])?,
                        b: c(parts[2])?,
                        a: alpha.clamp(0., 1.) as f32,
                    })
                }
                "hsl" | "hsla" => {
                    let h: f64 = parts[0].trim_end_matches("deg").parse().ok()?;
                    let sat = parse_unit(parts[1], 1.)?;
                    let l = parse_unit(parts[2], 1.)?;
                    let mut c = hsl(h, sat, l);
                    c.a = alpha.clamp(0., 1.) as f32;
                    Some(c)
                }
                _ => None,
            };
        }
        if lower == "transparent" {
            return Some(Rgba::TRANSPARENT);
        }
        named(&lower).map(Rgba::hex_rgb)
    }

    /// `#rrggbb` (alpha is written separately as an opacity attribute).
    pub fn hex(&self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    pub fn with_alpha(self, a: f64) -> Rgba {
        Rgba {
            a: a.clamp(0., 1.) as f32,
            ..self
        }
    }

    pub fn scale_alpha(self, f: f64) -> Rgba {
        self.with_alpha(self.a as f64 * f)
    }

    /// Relative luminance (WCAG), 0..=1.
    pub fn luminance(&self) -> f64 {
        let ch = |c: u8| {
            let c = c as f64 / 255.;
            if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        0.2126 * ch(self.r) + 0.7152 * ch(self.g) + 0.0722 * ch(self.b)
    }

    /// tinycolor's `isLight()` (perceived brightness > 128), which plotly
    /// uses to pick `#444` or white text over a fill.
    pub fn is_light(&self) -> bool {
        (self.r as f64 * 299. + self.g as f64 * 587. + self.b as f64 * 114.) / 1000. > 128.
    }

    /// plotly's `Color.contrast`: text color readable over `self`.
    pub fn contrast_text(&self) -> Rgba {
        if self.is_light() { Rgba::hex_rgb(0x444444) } else { Rgba::WHITE }
    }

    /// Linear RGB(A) interpolation, `t` in 0..=1.
    pub fn mix(a: Rgba, b: Rgba, t: f64) -> Rgba {
        let t = t.clamp(0., 1.);
        let l = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round().clamp(0., 255.) as u8;
        Rgba {
            r: l(a.r, b.r),
            g: l(a.g, b.g),
            b: l(a.b, b.b),
            a: (a.a as f64 + (b.a as f64 - a.a as f64) * t) as f32,
        }
    }
}

fn parse_hex(hex: &str) -> Option<Rgba> {
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = |s: &str| u8::from_str_radix(s, 16).ok();
    let d = |c: &str| v(c).map(|x| x * 17);
    match hex.len() {
        3 => Some(Rgba::rgb(d(&hex[0..1])?, d(&hex[1..2])?, d(&hex[2..3])?)),
        4 => Some(Rgba {
            a: d(&hex[3..4])? as f32 / 255.,
            ..Rgba::rgb(d(&hex[0..1])?, d(&hex[1..2])?, d(&hex[2..3])?)
        }),
        6 => Some(Rgba::rgb(v(&hex[0..2])?, v(&hex[2..4])?, v(&hex[4..6])?)),
        8 => Some(Rgba {
            a: v(&hex[6..8])? as f32 / 255.,
            ..Rgba::rgb(v(&hex[0..2])?, v(&hex[2..4])?, v(&hex[4..6])?)
        }),
        _ => None,
    }
}

/// `"50%"` -> 0.5 * scale... returns value in `scale` units.
fn parse_unit(p: &str, scale: f64) -> Option<f64> {
    let v = if let Some(pct) = p.strip_suffix('%') {
        pct.trim().parse::<f64>().ok()? / 100. * scale
    } else {
        p.trim().parse::<f64>().ok()?
    };
    v.is_finite().then_some(v)
}

fn hsl(h: f64, s: f64, l: f64) -> Rgba {
    let h = h.rem_euclid(360.) / 360.;
    let s = s.clamp(0., 1.);
    let l = l.clamp(0., 1.);
    if s == 0. {
        let v = (l * 255.).round() as u8;
        return Rgba::rgb(v, v, v);
    }
    let q = if l < 0.5 { l * (1. + s) } else { l + s - l * s };
    let p = 2. * l - q;
    let f = |mut t: f64| {
        if t < 0. {
            t += 1.
        }
        if t > 1. {
            t -= 1.
        }
        let v = if t < 1. / 6. {
            p + (q - p) * 6. * t
        } else if t < 0.5 {
            q
        } else if t < 2. / 3. {
            p + (q - p) * (2. / 3. - t) * 6.
        } else {
            p
        };
        (v * 255.).round().clamp(0., 255.) as u8
    };
    Rgba::rgb(f(h + 1. / 3.), f(h), f(h - 1. / 3.))
}

/// A continuous colorscale: sorted (position 0..=1, color) stops.
#[derive(Clone, Debug)]
pub struct Colorscale {
    pub stops: Vec<(f64, Rgba)>,
}

impl Colorscale {
    pub fn from_hex_samples(hex: &str) -> Option<Colorscale> {
        let n = hex.len() / 6;
        if n < 2 {
            return None;
        }
        let mut stops = Vec::with_capacity(n);
        for i in 0..n {
            let c = parse_hex(&hex[i * 6..i * 6 + 6])?;
            stops.push((i as f64 / (n - 1) as f64, c));
        }
        Some(Colorscale { stops })
    }

    pub fn from_stops(stops: Vec<(f64, Rgba)>) -> Option<Colorscale> {
        let mut stops: Vec<(f64, Rgba)> = stops.into_iter().filter(|(p, _)| p.is_finite()).collect();
        if stops.len() < 2 {
            if stops.len() == 1 {
                let c = stops[0].1;
                return Some(Colorscale { stops: vec![(0., c), (1., c)] });
            }
            return None;
        }
        stops.sort_by(|a, b| a.0.total_cmp(&b.0));
        Some(Colorscale { stops })
    }

    pub fn reversed(&self) -> Colorscale {
        Colorscale {
            stops: self.stops.iter().rev().map(|&(p, c)| (1. - p, c)).collect(),
        }
    }

    pub fn at(&self, t: f64) -> Rgba {
        let t = if t.is_finite() { t.clamp(0., 1.) } else { 0. };
        let s = &self.stops;
        if t <= s[0].0 {
            return s[0].1;
        }
        for w in s.windows(2) {
            let (p0, c0) = w[0];
            let (p1, c1) = w[1];
            if t <= p1 {
                let span = p1 - p0;
                return if span <= 0. { c1 } else { Rgba::mix(c0, c1, (t - p0) / span) };
            }
        }
        s[s.len() - 1].1
    }

    /// plotly.js built-in scale by name (case-insensitive).
    pub fn plotly_named(name: &str) -> Option<Colorscale> {
        let (_, stops) = schemes::PLOTLY_SCALES.iter().find(|(n, _)| n.eq_ignore_ascii_case(name))?;
        Colorscale::from_stops(stops.iter().filter_map(|&(p, c)| Some((p, Rgba::parse(c)?))).collect())
    }

    /// Vega continuous scheme by name (case-insensitive).
    pub fn vega_named(name: &str) -> Option<Colorscale> {
        let lower = name.to_ascii_lowercase();
        let (_, hex) = schemes::VEGA_CONTINUOUS.iter().find(|(n, _)| *n == lower)?;
        Colorscale::from_hex_samples(hex)
    }
}

/// Vega discrete scheme colors by name.
pub fn vega_discrete(name: &str) -> Option<Vec<Rgba>> {
    let lower = name.to_ascii_lowercase();
    let (_, hex) = schemes::VEGA_DISCRETE.iter().find(|(n, _)| *n == lower)?;
    (0..hex.len() / 6).map(|i| parse_hex(&hex[i * 6..i * 6 + 6])).collect()
}

/// CSS named colors.
pub fn named(name: &str) -> Option<u32> {
    let i = NAMED.binary_search_by(|(n, _)| n.cmp(&name)).ok()?;
    Some(NAMED[i].1)
}

const NAMED: &[(&str, u32)] = &[
    ("aliceblue", 0xf0f8ff),
    ("antiquewhite", 0xfaebd7),
    ("aqua", 0x00ffff),
    ("aquamarine", 0x7fffd4),
    ("azure", 0xf0ffff),
    ("beige", 0xf5f5dc),
    ("bisque", 0xffe4c4),
    ("black", 0x000000),
    ("blanchedalmond", 0xffebcd),
    ("blue", 0x0000ff),
    ("blueviolet", 0x8a2be2),
    ("brown", 0xa52a2a),
    ("burlywood", 0xdeb887),
    ("cadetblue", 0x5f9ea0),
    ("chartreuse", 0x7fff00),
    ("chocolate", 0xd2691e),
    ("coral", 0xff7f50),
    ("cornflowerblue", 0x6495ed),
    ("cornsilk", 0xfff8dc),
    ("crimson", 0xdc143c),
    ("cyan", 0x00ffff),
    ("darkblue", 0x00008b),
    ("darkcyan", 0x008b8b),
    ("darkgoldenrod", 0xb8860b),
    ("darkgray", 0xa9a9a9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xa9a9a9),
    ("darkkhaki", 0xbdb76b),
    ("darkmagenta", 0x8b008b),
    ("darkolivegreen", 0x556b2f),
    ("darkorange", 0xff8c00),
    ("darkorchid", 0x9932cc),
    ("darkred", 0x8b0000),
    ("darksalmon", 0xe9967a),
    ("darkseagreen", 0x8fbc8f),
    ("darkslateblue", 0x483d8b),
    ("darkslategray", 0x2f4f4f),
    ("darkslategrey", 0x2f4f4f),
    ("darkturquoise", 0x00ced1),
    ("darkviolet", 0x9400d3),
    ("deeppink", 0xff1493),
    ("deepskyblue", 0x00bfff),
    ("dimgray", 0x696969),
    ("dimgrey", 0x696969),
    ("dodgerblue", 0x1e90ff),
    ("firebrick", 0xb22222),
    ("floralwhite", 0xfffaf0),
    ("forestgreen", 0x228b22),
    ("fuchsia", 0xff00ff),
    ("gainsboro", 0xdcdcdc),
    ("ghostwhite", 0xf8f8ff),
    ("gold", 0xffd700),
    ("goldenrod", 0xdaa520),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("greenyellow", 0xadff2f),
    ("grey", 0x808080),
    ("honeydew", 0xf0fff0),
    ("hotpink", 0xff69b4),
    ("indianred", 0xcd5c5c),
    ("indigo", 0x4b0082),
    ("ivory", 0xfffff0),
    ("khaki", 0xf0e68c),
    ("lavender", 0xe6e6fa),
    ("lavenderblush", 0xfff0f5),
    ("lawngreen", 0x7cfc00),
    ("lemonchiffon", 0xfffacd),
    ("lightblue", 0xadd8e6),
    ("lightcoral", 0xf08080),
    ("lightcyan", 0xe0ffff),
    ("lightgoldenrodyellow", 0xfafad2),
    ("lightgray", 0xd3d3d3),
    ("lightgreen", 0x90ee90),
    ("lightgrey", 0xd3d3d3),
    ("lightpink", 0xffb6c1),
    ("lightsalmon", 0xffa07a),
    ("lightseagreen", 0x20b2aa),
    ("lightskyblue", 0x87cefa),
    ("lightslategray", 0x778899),
    ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xb0c4de),
    ("lightyellow", 0xffffe0),
    ("lime", 0x00ff00),
    ("limegreen", 0x32cd32),
    ("linen", 0xfaf0e6),
    ("magenta", 0xff00ff),
    ("maroon", 0x800000),
    ("mediumaquamarine", 0x66cdaa),
    ("mediumblue", 0x0000cd),
    ("mediumorchid", 0xba55d3),
    ("mediumpurple", 0x9370db),
    ("mediumseagreen", 0x3cb371),
    ("mediumslateblue", 0x7b68ee),
    ("mediumspringgreen", 0x00fa9a),
    ("mediumturquoise", 0x48d1cc),
    ("mediumvioletred", 0xc71585),
    ("midnightblue", 0x191970),
    ("mintcream", 0xf5fffa),
    ("mistyrose", 0xffe4e1),
    ("moccasin", 0xffe4b5),
    ("navajowhite", 0xffdead),
    ("navy", 0x000080),
    ("oldlace", 0xfdf5e6),
    ("olive", 0x808000),
    ("olivedrab", 0x6b8e23),
    ("orange", 0xffa500),
    ("orangered", 0xff4500),
    ("orchid", 0xda70d6),
    ("palegoldenrod", 0xeee8aa),
    ("palegreen", 0x98fb98),
    ("paleturquoise", 0xafeeee),
    ("palevioletred", 0xdb7093),
    ("papayawhip", 0xffefd5),
    ("peachpuff", 0xffdab9),
    ("peru", 0xcd853f),
    ("pink", 0xffc0cb),
    ("plum", 0xdda0dd),
    ("powderblue", 0xb0e0e6),
    ("purple", 0x800080),
    ("rebeccapurple", 0x663399),
    ("red", 0xff0000),
    ("rosybrown", 0xbc8f8f),
    ("royalblue", 0x4169e1),
    ("saddlebrown", 0x8b4513),
    ("salmon", 0xfa8072),
    ("sandybrown", 0xf4a460),
    ("seagreen", 0x2e8b57),
    ("seashell", 0xfff5ee),
    ("sienna", 0xa0522d),
    ("silver", 0xc0c0c0),
    ("skyblue", 0x87ceeb),
    ("slateblue", 0x6a5acd),
    ("slategray", 0x708090),
    ("slategrey", 0x708090),
    ("snow", 0xfffafa),
    ("springgreen", 0x00ff7f),
    ("steelblue", 0x4682b4),
    ("tan", 0xd2b48c),
    ("teal", 0x008080),
    ("thistle", 0xd8bfd8),
    ("tomato", 0xff6347),
    ("turquoise", 0x40e0d0),
    ("violet", 0xee82ee),
    ("wheat", 0xf5deb3),
    ("white", 0xffffff),
    ("whitesmoke", 0xf5f5f5),
    ("yellow", 0xffff00),
    ("yellowgreen", 0x9acd32),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_css_colors() {
        assert_eq!(Rgba::parse("#636efa"), Some(Rgba::rgb(0x63, 0x6e, 0xfa)));
        assert_eq!(Rgba::parse("#abc"), Some(Rgba::rgb(0xaa, 0xbb, 0xcc)));
        assert_eq!(Rgba::parse("rgb(5,10,172)"), Some(Rgba::rgb(5, 10, 172)));
        let c = Rgba::parse("rgba(255, 0, 0, 0.5)").unwrap();
        assert_eq!((c.r, c.a), (255, 0.5));
        assert_eq!(Rgba::parse("Red"), Some(Rgba::rgb(255, 0, 0)));
        assert_eq!(Rgba::parse("hsl(120, 100%, 25%)"), Some(Rgba::rgb(0, 128, 0)));
        assert_eq!(Rgba::parse("nope"), None);
        assert_eq!(Rgba::parse("#12345"), None);
        assert_eq!(Rgba::parse("rgb(1,2)"), None);
    }

    #[test]
    fn named_table_is_sorted() {
        assert!(NAMED.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn colorscale_interpolates() {
        let s = Colorscale::plotly_named("Greys").unwrap();
        assert_eq!(s.at(0.), Rgba::BLACK);
        assert_eq!(s.at(1.), Rgba::WHITE);
        assert_eq!(s.at(0.5), Rgba::rgb(128, 128, 128));
        assert!(Colorscale::vega_named("viridis").is_some());
        assert_eq!(vega_discrete("tableau10").unwrap()[0], Rgba::hex_rgb(0x4c78a8));
    }
}
