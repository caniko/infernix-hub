//! Stylesheet assembly.
//!
//! Desktop has no `dx` bundler, so the CSS is embedded with `include_str!` and
//! the three OFL variable fonts are inlined as `data:` URIs. The donor injected
//! `@font-face` rules from `asset!()` URLs; this keeps the same offline promise
//! without a CLI step.
//!
//! The palette can be overridden by a JSON file written by the operator's
//! desktop theme tooling (`HUB_THEME_FILE`, else the XDG config path). The
//! values are interpolated into CSS, so they are validated as `#rrggbb`.

use std::path::PathBuf;
use std::sync::OnceLock;

const THEME_CSS: &str = include_str!("theme.css");

/// Theme-file key → CSS custom property. Every key is optional.
const PALETTE: &[(&str, &str)] = &[
    ("bg", "--bg"),
    ("panel", "--panel"),
    ("card", "--card"),
    ("elevated", "--elevated"),
    ("border", "--border"),
    ("text", "--text"),
    ("textMuted", "--text-muted"),
    ("accent", "--honey"),
    ("focus", "--focus"),
    ("ok", "--ok"),
    ("warn", "--warn"),
    ("bad", "--bad"),
    ("info", "--info"),
];

const FONT_DISPLAY: &[u8] = include_bytes!("../assets/fonts/space-grotesk-var.woff2");
const FONT_BODY: &[u8] = include_bytes!("../assets/fonts/inter-var.woff2");
const FONT_MONO: &[u8] = include_bytes!("../assets/fonts/jetbrains-mono-var.woff2");

pub type Rgba = (u8, u8, u8, u8);

const DEFAULT_BG_RGBA: Rgba = (0, 0, 0, 255);

struct ResolvedTheme {
    css: String,
    bg_rgba: Rgba,
}

fn resolved() -> &'static ResolvedTheme {
    static THEME: OnceLock<ResolvedTheme> = OnceLock::new();
    THEME.get_or_init(|| {
        let (overrides, bg_rgba) = load_overrides();
        ResolvedTheme { css: format!("{}{}{}", font_faces(), THEME_CSS, overrides), bg_rgba }
    })
}

/// `@font-face` rules with the font bytes inlined, followed by the theme and
/// the palette overrides (a second `:root` block, so only provided keys win).
/// Built once per process so poll re-renders never re-encode the embedded fonts.
pub fn style_sheet() -> &'static str {
    &resolved().css
}

/// Resolved window background color (`--bg`), used before the webview paints to
/// avoid a white startup flash.
pub fn window_background_rgba() -> Rgba {
    resolved().bg_rgba
}

/// Whether the active palette has a dark background, used to pick the matching
/// window decoration theme on GTK/Windows/macOS.
pub fn is_dark_theme() -> bool {
    is_dark_rgb(resolved().bg_rgba)
}

fn is_dark_rgb((r, g, b, _): Rgba) -> bool {
    (0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b)) < 128.0
}

fn load_overrides() -> (String, Rgba) {
    let Some(path) = theme_path() else { return (String::new(), DEFAULT_BG_RGBA) };
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(error) => {
            eprintln!("theme: cannot read {} ({error}); using the built-in palette", path.display());
            return (String::new(), DEFAULT_BG_RGBA);
        }
    };
    match parse_theme_json(&body) {
        Ok((css, bg)) => (css, bg.unwrap_or(DEFAULT_BG_RGBA)),
        Err(error) => {
            eprintln!("theme: {} is malformed ({error}); using the built-in palette", path.display());
            (String::new(), DEFAULT_BG_RGBA)
        }
    }
}

fn theme_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("HUB_THEME_FILE").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some(config.join("fleetix-hub").join("theme.json"))
}

/// Build the `:root{...}` override block and resolve `--bg` if valid. Only
/// `#rrggbb` strings survive; the file is operator-owned but its values become
/// CSS, so this is a trust boundary. Unknown keys and invalid values are dropped.
fn parse_theme_json(body: &str) -> Result<(String, Option<Rgba>), String> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|error| error.to_string())?;
    let Some(object) = value.as_object() else {
        return Err("the theme file is not a JSON object".into());
    };
    let mut declarations = String::new();
    let mut bg = None;
    for (key, variable) in PALETTE {
        let Some(color) = object.get(*key).and_then(|value| value.as_str()) else { continue };
        let Some(rgba) = parse_hex_rgba(color) else { continue };
        if *key == "bg" {
            bg = Some(rgba);
        }
        declarations.push_str(&format!("{variable}:{color};"));
    }
    if bg.is_some_and(|rgba| !is_dark_rgb(rgba)) {
        declarations
            .push_str("--color-scheme: light;--text-dim:var(--text-muted);--line-subtle:color-mix(in srgb,var(--text) 12%,transparent);");
    }
    let css = if declarations.is_empty() { String::new() } else { format!(":root{{{declarations}}}") };
    Ok((css, bg))
}

#[cfg(test)]
fn overrides_from_json(body: &str) -> Result<String, String> {
    parse_theme_json(body).map(|(css, _)| css)
}

fn parse_hex_rgba(value: &str) -> Option<Rgba> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b, 255))
}

fn font_faces() -> String {
    format!(
        "@font-face{{font-family:'Space Grotesk';src:url(data:font/woff2;base64,{}) format('woff2');font-weight:300 700;font-display:swap;}}\
         @font-face{{font-family:'Inter';src:url(data:font/woff2;base64,{}) format('woff2');font-weight:100 900;font-display:swap;}}\
         @font-face{{font-family:'JetBrains Mono';src:url(data:font/woff2;base64,{}) format('woff2');font-weight:100 800;font-display:swap;}}",
        base64(FONT_DISPLAY),
        base64(FONT_BODY),
        base64(FONT_MONO),
    )
}

/// Minimal standard base64 (RFC 4648, padded). Avoids a dependency for three
/// static font blobs.
pub(crate) fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (plain, encoded) in
            [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")]
        {
            assert_eq!(base64(plain.as_bytes()), encoded);
        }
    }

    #[test]
    fn stylesheet_carries_fonts_and_tokens() {
        let css = style_sheet();
        assert!(css.starts_with("@font-face"));
        assert!(css.contains("data:font/woff2;base64,"));
        assert!(css.contains("--honey: #e5a72b"));
        assert!(css.contains("prefers-reduced-motion"));
    }

    #[test]
    fn palette_overrides_accept_only_hex_colors() {
        let css = overrides_from_json(r##"{"bg": "#f7f4ec", "textMuted": "#123ABC", "accent": "#a86f00"}"##).unwrap();
        assert_eq!(
            css,
            concat!(
                ":root{--bg:#f7f4ec;--text-muted:#123ABC;--honey:#a86f00;",
                "--color-scheme: light;--text-dim:var(--text-muted);--line-subtle:color-mix(in srgb,var(--text) 12%,transparent);}"
            )
        );

        // Injection, wrong shapes and unknown keys are dropped, not escaped.
        let hostile = r##"{"bg": "red;} body{display:none", "panel": "#12345", "card": "#gggggg", "text": 7, "mystery": "#ffffff"}"##;
        assert_eq!(overrides_from_json(hostile).unwrap(), "");

        assert_eq!(overrides_from_json("{}").unwrap(), "");
        assert!(overrides_from_json("[]").is_err());
        assert!(overrides_from_json("not json").is_err());
    }

    #[test]
    fn stylesheet_is_cached_and_bg_luminance_classifies_theme() {
        assert!(std::ptr::eq(style_sheet(), style_sheet()));
        assert_eq!(parse_hex_rgba("#181714"), Some((0x18, 0x17, 0x14, 255)));
        assert_eq!(parse_hex_rgba("#f7f4ec"), Some((0xf7, 0xf4, 0xec, 255)));
        assert_eq!(parse_hex_rgba("not-hex"), None);
        assert!(is_dark_rgb(DEFAULT_BG_RGBA));
        assert!(!is_dark_rgb((0xf7, 0xf4, 0xec, 255)));
    }

    #[test]
    fn native_controls_follow_the_palette() {
        assert!(THEME_CSS.contains(":root, input, textarea, select, button { color-scheme: var(--color-scheme, dark); }"));
        assert!(overrides_from_json(r##"{"bg":"#f7f4ec"}"##).unwrap().contains("--color-scheme: light;"));
        assert!(!overrides_from_json(r##"{"bg":"#181714"}"##).unwrap().contains("--color-scheme: light;"));
    }
}
