//! Stylesheet assembly.
//!
//! Desktop has no `dx` bundler, so the CSS is embedded with `include_str!` and
//! the three OFL variable fonts are inlined as `data:` URIs. The donor injected
//! `@font-face` rules from `asset!()` URLs; this keeps the same offline promise
//! without a CLI step.

const THEME_CSS: &str = include_str!("theme.css");

const FONT_DISPLAY: &[u8] = include_bytes!("../assets/fonts/space-grotesk-var.woff2");
const FONT_BODY: &[u8] = include_bytes!("../assets/fonts/inter-var.woff2");
const FONT_MONO: &[u8] = include_bytes!("../assets/fonts/jetbrains-mono-var.woff2");

/// `@font-face` rules with the font bytes inlined, followed by the theme.
pub fn style_sheet() -> String {
    format!("{}{}", font_faces(), THEME_CSS)
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
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
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
}
