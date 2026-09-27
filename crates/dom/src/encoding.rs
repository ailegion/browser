//! Character encoding of documents and stylesheets, on `encoding_rs`.
//!
//! HTML follows "determining the character encoding" in the HTML standard:
//! byte order mark, then the transport layer's charset, then a prescan of
//! the first kilobyte for `<meta charset>` or `<meta http-equiv>`, then a
//! default. The default is UTF-8 unless the prescanned bytes are not valid
//! UTF-8, in which case windows-1252, the most common legacy encoding and
//! the standard's default for most locales. Stylesheets follow CSS Syntax 3
//! "decode bytes": byte order mark, transport charset, `@charset`, UTF-8.

use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252};

/// Bytes examined before an undeclared document's encoding is decided.
pub const PRESCAN_BYTES: usize = 1024;

/// Choose the encoding of an HTML byte stream from what has arrived so
/// far. `transport` is the charset parameter of the Content-Type header.
pub fn sniff_html(head: &[u8], transport: Option<&str>) -> &'static Encoding {
    if let Some((enc, _)) = Encoding::for_bom(head) {
        return enc;
    }
    if let Some(enc) = transport.and_then(|l| Encoding::for_label(l.trim().as_bytes())) {
        return enc;
    }
    let window = &head[..head.len().min(PRESCAN_BYTES)];
    if let Some(enc) = prescan_meta(window) {
        return enc;
    }
    if looks_like_utf8(window) { UTF_8 } else { WINDOWS_1252 }
}

/// Decode an external script's bytes to text: byte order mark, then the
/// transport charset, then UTF-8, per the HTML standard's "decode" for
/// classic scripts (modules are always UTF-8, which the same steps yield
/// when no charset is passed).
pub fn decode_script(bytes: &[u8], transport: Option<&str>) -> String {
    let enc = transport
        .and_then(|l| Encoding::for_label(l.trim().as_bytes()))
        .unwrap_or(UTF_8);
    enc.decode(bytes).0.into_owned()
}

/// Decode a stylesheet's bytes to text.
pub fn decode_stylesheet(bytes: &[u8], transport: Option<&str>) -> String {
    let enc = transport
        .and_then(|l| Encoding::for_label(l.trim().as_bytes()))
        .or_else(|| css_charset_rule(bytes))
        .unwrap_or(UTF_8);
    // `decode` sniffs the byte order mark first and lets it win.
    enc.decode(bytes).0.into_owned()
}

/// `@charset "label";` at the very start of the sheet, byte for byte.
fn css_charset_rule(bytes: &[u8]) -> Option<&'static Encoding> {
    let rest = bytes.strip_prefix(b"@charset \"")?;
    let end = rest.iter().position(|&b| b == b'"')?;
    if rest.get(end + 1) != Some(&b';') {
        return None;
    }
    Some(no_utf16(Encoding::for_label(&rest[..end])?))
}

/// Valid UTF-8, allowing a multi-byte sequence cut off at the end.
fn looks_like_utf8(bytes: &[u8]) -> bool {
    match std::str::from_utf8(bytes) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    }
}

/// A UTF-16 label in a document or sheet means UTF-8, per both standards
/// (the bytes evidently were not UTF-16, or the BOM would have said so).
fn no_utf16(enc: &'static Encoding) -> &'static Encoding {
    if enc == UTF_16BE || enc == UTF_16LE { UTF_8 } else { enc }
}

fn is_ws(b: u8) -> bool {
    matches!(b, b'\t' | b'\n' | 0x0C | b'\r' | b' ')
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn find_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w.eq_ignore_ascii_case(needle))
}

/// "Prescan a byte stream to determine its encoding" from the HTML
/// standard, over the window it is given.
fn prescan_meta(bytes: &[u8]) -> Option<&'static Encoding> {
    let len = bytes.len();
    let mut pos = 0;
    while pos < len {
        if bytes[pos] != b'<' {
            pos += 1;
            continue;
        }
        let rest = &bytes[pos..];
        if rest.starts_with(b"<!--") {
            // "<!-->" counts as a complete comment: search from after "<!".
            match find(&bytes[pos + 2..], b"-->") {
                Some(i) => pos += 2 + i + 3,
                None => return None,
            }
        } else if rest.len() > 5 && rest[..5].eq_ignore_ascii_case(b"<meta") && (is_ws(rest[5]) || rest[5] == b'/') {
            pos += 5;
            let mut seen: Vec<Vec<u8>> = Vec::new();
            let mut got_pragma = false;
            let mut need_pragma: Option<bool> = None;
            let mut charset: Option<&'static Encoding> = None;
            while let Some((name, value, next)) = get_attribute(bytes, pos) {
                pos = next;
                if seen.contains(&name) {
                    continue;
                }
                match name.as_slice() {
                    b"http-equiv" => {
                        if value.eq_ignore_ascii_case(b"content-type") {
                            got_pragma = true;
                        }
                    }
                    b"content" => {
                        if charset.is_none()
                            && let Some(enc) = charset_from_content(&value)
                        {
                            charset = Some(enc);
                            need_pragma = Some(true);
                        }
                    }
                    b"charset" => {
                        charset = Encoding::for_label(&value);
                        need_pragma = Some(false);
                    }
                    _ => {}
                }
                seen.push(name);
            }
            match (need_pragma, charset) {
                (None, _) | (_, None) => continue,
                (Some(true), _) if !got_pragma => continue,
                (Some(_), Some(enc)) => {
                    return Some(if enc.name() == "x-user-defined" { WINDOWS_1252 } else { no_utf16(enc) });
                }
            }
        } else if rest.len() > 1
            && (rest[1].is_ascii_alphabetic() || (rest[1] == b'/' && rest.len() > 2 && rest[2].is_ascii_alphabetic()))
        {
            // Any other tag: skip its name and attributes.
            pos += 1;
            while pos < len && !is_ws(bytes[pos]) && bytes[pos] != b'>' {
                pos += 1;
            }
            while let Some((_, _, next)) = get_attribute(bytes, pos) {
                pos = next;
            }
            if pos < len && bytes[pos] == b'>' {
                pos += 1;
            }
        } else if rest.starts_with(b"<!") || rest.starts_with(b"</") || rest.starts_with(b"<?") {
            match bytes[pos..].iter().position(|&b| b == b'>') {
                Some(i) => pos += i + 1,
                None => return None,
            }
        } else {
            pos += 1;
        }
    }
    None
}

/// "Get an attribute" from the prescan algorithm. Returns the lowercased
/// name and value and the position after them, or `None` at `>` or the
/// end of the window.
fn get_attribute(bytes: &[u8], mut pos: usize) -> Option<(Vec<u8>, Vec<u8>, usize)> {
    let len = bytes.len();
    while pos < len && (is_ws(bytes[pos]) || bytes[pos] == b'/') {
        pos += 1;
    }
    if pos >= len || bytes[pos] == b'>' {
        return None;
    }
    let mut name = Vec::new();
    let mut value = Vec::new();
    loop {
        if pos >= len {
            return None;
        }
        let b = bytes[pos];
        if b == b'=' && !name.is_empty() {
            pos += 1;
            break;
        }
        if is_ws(b) {
            while pos < len && is_ws(bytes[pos]) {
                pos += 1;
            }
            if pos >= len {
                return None;
            }
            if bytes[pos] != b'=' {
                return Some((name, value, pos));
            }
            pos += 1;
            break;
        }
        if b == b'/' || b == b'>' {
            return Some((name, value, pos));
        }
        name.push(b.to_ascii_lowercase());
        pos += 1;
    }
    while pos < len && is_ws(bytes[pos]) {
        pos += 1;
    }
    if pos >= len {
        return None;
    }
    match bytes[pos] {
        q @ (b'"' | b'\'') => {
            pos += 1;
            loop {
                if pos >= len {
                    return None;
                }
                let b = bytes[pos];
                pos += 1;
                if b == q {
                    return Some((name, value, pos));
                }
                value.push(b.to_ascii_lowercase());
            }
        }
        b'>' => Some((name, value, pos)),
        _ => loop {
            if pos >= len {
                return None;
            }
            let b = bytes[pos];
            if is_ws(b) || b == b'>' {
                return Some((name, value, pos));
            }
            value.push(b.to_ascii_lowercase());
            pos += 1;
        },
    }
}

/// "Extracting a character encoding from a meta element": the `charset=`
/// inside a `content` attribute value.
fn charset_from_content(content: &[u8]) -> Option<&'static Encoding> {
    let len = content.len();
    let mut pos = 0;
    loop {
        pos += find_ci(&content[pos..], b"charset")? + 7;
        while pos < len && is_ws(content[pos]) {
            pos += 1;
        }
        if pos < len && content[pos] == b'=' {
            pos += 1;
            break;
        }
    }
    while pos < len && is_ws(content[pos]) {
        pos += 1;
    }
    if pos >= len {
        return None;
    }
    let label = match content[pos] {
        q @ (b'"' | b'\'') => {
            let end = content[pos + 1..].iter().position(|&b| b == q)?;
            &content[pos + 1..pos + 1 + end]
        }
        _ => {
            let end = content[pos..]
                .iter()
                .position(|&b| is_ws(b) || b == b';')
                .map_or(len, |e| pos + e);
            &content[pos..end]
        }
    };
    Encoding::for_label(label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::{ISO_8859_2, SHIFT_JIS, UTF_16LE};

    #[test]
    fn precedence_bom_transport_meta_default() {
        assert_eq!(sniff_html(b"\xff\xfe<\0m\0", Some("shift_jis")), UTF_16LE);
        assert_eq!(sniff_html(b"<meta charset=iso-8859-2>", Some("Shift_JIS")), SHIFT_JIS);
        assert_eq!(sniff_html(b"<html><head><meta charset=iso-8859-2>", None), ISO_8859_2);
        assert_eq!(sniff_html(b"<p>plain ascii", None), UTF_8);
        assert_eq!(sniff_html("<p>caf\u{e9}".as_bytes(), None), UTF_8);
        assert_eq!(sniff_html(b"<p>caf\xe9 au lait", None), WINDOWS_1252);
        // A multi-byte sequence cut at the window end is still UTF-8.
        let mut cut = vec![b'x'; PRESCAN_BYTES - 1];
        cut.extend_from_slice("\u{e9}".as_bytes());
        cut.extend_from_slice(b"more");
        assert_eq!(sniff_html(&cut, None), UTF_8);
        // Bogus transport label falls through to the prescan.
        assert_eq!(sniff_html(b"<meta charset=shift_jis>", Some("bogus")), SHIFT_JIS);
    }

    #[test]
    fn meta_forms() {
        let cases: &[(&[u8], &'static Encoding)] = &[
            (b"<META CHARSET=\"Shift_JIS\">", SHIFT_JIS),
            (b"<meta charset='shift_jis' >", SHIFT_JIS),
            (b"<meta http-equiv=\"Content-Type\" content=\"text/html; charset=shift_jis\">", SHIFT_JIS),
            (b"<meta content=\"text/html; charset = 'shift_jis'\" http-equiv=content-type>", SHIFT_JIS),
            (b"<meta http-equiv=content-type content='charset=shift_jis;x=y'>", SHIFT_JIS),
            (b"<meta/charset=shift_jis>", SHIFT_JIS),
            (b"<meta name=viewport content='width=device-width'><meta charset=shift_jis>", SHIFT_JIS),
            (b"<div data-x='<meta charset=shift_jis>'></div><meta charset=iso-8859-2>", ISO_8859_2),
            (b"<!-- <meta charset=shift_jis> --><meta charset=iso-8859-2>", ISO_8859_2),
            (b"<!--><meta charset=shift_jis>", SHIFT_JIS),
            (b"<meta charset=utf-16le>", UTF_8),
            (b"<meta charset=x-user-defined>", WINDOWS_1252),
            // Content without the pragma does not count; the first
            // charset attribute wins over a later one.
            (b"<meta content='charset=shift_jis'><meta charset=iso-8859-2>", ISO_8859_2),
            (b"<meta charset=shift_jis charset=iso-8859-2>", SHIFT_JIS),
        ];
        for (html, want) in cases {
            assert_eq!(sniff_html(html, None), *want, "{}", String::from_utf8_lossy(html));
        }
        assert_eq!(sniff_html(b"<meta charset=unknown-thing>", None), UTF_8);
        assert_eq!(sniff_html(b"<meta name=x content=y", None), UTF_8);
    }

    #[test]
    fn stylesheet_decoding() {
        assert_eq!(decode_stylesheet(b"@charset \"windows-1252\"; a { content: '\xe9' }", None), "@charset \"windows-1252\"; a { content: '\u{e9}' }");
        assert_eq!(decode_stylesheet(b"a { content: '\xe9' }", Some("windows-1252")), "a { content: '\u{e9}' }");
        assert_eq!(decode_stylesheet(b"\xef\xbb\xbfa{}", Some("windows-1252")), "a{}");
        assert_eq!(decode_stylesheet(b"a { content: '\xe9' }", None), "a { content: '\u{fffd}' }");
        assert_eq!(decode_stylesheet(b"@charset \"utf-16le\"; a{}", None), "@charset \"utf-16le\"; a{}");
    }

    #[test]
    fn garbage_windows_do_not_panic() {
        let cases: &[&[u8]] = &[b"<", b"<meta", b"<meta ", b"<meta charset", b"<meta charset=", b"<meta charset=\"", b"<!--", b"<!", b"</", b"<a b='", b"<meta http-equiv=content-type content=charset", b"\xff\xfe", b"<meta charset=", b"<meta content='charset=' http-equiv=content-type>"];
        for c in cases {
            let _ = sniff_html(c, None);
            let _ = sniff_html(c, Some("utf-8"));
        }
    }
}
