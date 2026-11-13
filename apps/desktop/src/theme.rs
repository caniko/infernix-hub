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
    ("textMuted", "--text-2"),
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

/// `@font-face` rules with the font bytes inlined, followed by the theme and
/// the palette overrides (a second `:root` block, so only provided keys win).
pub fn style_sheet() -> String {
    format!("{}{}{}", font_faces(), THEME_CSS, palette_overrides())
}

/// Read once per process: the stylesheet is rebuilt on every poll tick.
fn palette_overrides() -> &'static str {
    static OVERRIDES: OnceLock<String> = OnceLock::new();
    OVERRIDES.get_or_init(load_overrides)
}

fn load_overrides() -> String {
    let Some(path) = theme_path() else { return String::new() };
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(error) => {
            eprintln!("theme: cannot read {} ({error}); using the built-in palette", path.display());
            return String::new();
        }
    };
    match overrides_from_json(&body) {
        Ok(css) => css,
        Err(error) => {
            eprintln!("theme: {} is malformed ({error}); using the built-in palette", path.display());
            String::new()
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

/// Build the `:root{...}` override block. Only `#rrggbb` strings survive; the
/// file is operator-owned but its values become CSS, so this is a trust
/// boundary. Unknown keys and invalid values are dropped.
fn overrides_from_json(body: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(body).map_err(|error| error.to_string())?;
    let Some(object) = value.as_object() else {
        return Err("the theme file is not a JSON object".into());
    };
    let mut declarations = String::new();
    for (key, variable) in PALETTE {
        let Some(color) = object.get(*key).and_then(|value| value.as_str()) else { continue };
        if !is_hex_color(color) {
            continue;
        }
        declarations.push_str(&format!("{variable}:{color};"));
    }
    Ok(if declarations.is_empty() { String::new() } else { format!(":root{{{declarations}}}") })
}

fn is_hex_color(value: &str) -> bool {
    value.len() == 7 && value.starts_with('#') && value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
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
        assert_eq!(css, ":root{--bg:#f7f4ec;--text-2:#123ABC;--honey:#a86f00;}");

        // Injection, wrong shapes and unknown keys are dropped, not escaped.
        let hostile = r##"{"bg": "red;} body{display:none", "panel": "#12345", "card": "#gggggg", "text": 7, "mystery": "#ffffff"}"##;
        assert_eq!(overrides_from_json(hostile).unwrap(), "");

        assert_eq!(overrides_from_json("{}").unwrap(), "");
        assert!(overrides_from_json("[]").is_err());
        assert!(overrides_from_json("not json").is_err());
    }
}
