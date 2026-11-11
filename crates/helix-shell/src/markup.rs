//! Notification body markup (helix-desktop-spec
//! `contracts/dbus/org.freedesktop.Notifications.md`, "Body markup"):
//! `<b>`, `<i>`, `<u>`, `<a href>` and entities, into styled spans.

use helix_draw::Span;

#[derive(Default, Clone)]
struct Style {
    bold: u32,
    italic: u32,
    underline: u32,
    link: Vec<Option<String>>,
}

impl Style {
    fn span(&self, text: String) -> Span {
        Span {
            text,
            bold: self.bold > 0,
            underline: self.underline > 0,
            link: self.link.iter().rev().flatten().next().cloned(),
        }
    }
}

/// `&amp;` and friends, `&#N;` and `&#xN;`; anything else stays as is.
fn entity(s: &str) -> Option<(char, usize)> {
    let end = s.find(';')?;
    if end > 10 {
        return None;
    }
    let name = &s[1..end];
    let c = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => {
            let code = if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                name.strip_prefix('#')?.parse().ok()?
            };
            char::from_u32(code)?
        }
    };
    Some((c, end + 1))
}

/// The `href` of an `<a …>` tag's attributes.
fn href(attrs: &str) -> Option<String> {
    let at = attrs.find("href")?;
    let rest = attrs[at + 4..].trim_start().strip_prefix('=')?.trim_start();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let value = &rest[1..];
    let end = value.find(quote)?;
    Some(decode(&value[..end]))
}

/// Text with entities decoded.
fn decode(s: &str) -> String {
    let mut out = String::new();
    let mut i = 0;
    while let Some(c) = s[i..].chars().next() {
        if c == '&' {
            if let Some((ch, len)) = entity(&s[i..]) {
                out.push(ch);
                i += len;
                continue;
            }
        }
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// A tag at the start of `s` (which begins with `<`): (closing, name,
/// attributes, length). `None` if it is not a tag.
fn tag(s: &str) -> Option<(bool, String, String, usize)> {
    let end = s.find('>')?;
    let inner = &s[1..end];
    let (closing, inner) = match inner.strip_prefix('/') {
        Some(rest) => (true, rest),
        None => (false, inner),
    };
    let inner = inner.trim_end_matches('/').trim();
    let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
    let name = &inner[..name_end];
    if !name.starts_with(|c: char| c.is_ascii_alphabetic()) || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some((closing, name.to_ascii_lowercase(), inner[name_end..].to_owned(), end + 1))
}

/// Styled spans of a markup body.
pub fn parse(body: &str) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut style = Style::default();
    let mut text = String::new();
    let flush = |text: &mut String, style: &Style, spans: &mut Vec<Span>| {
        if !text.is_empty() {
            spans.push(style.span(std::mem::take(text)));
        }
    };
    let mut i = 0;
    while let Some(c) = body[i..].chars().next() {
        match c {
            '<' => match tag(&body[i..]) {
                Some((closing, name, attrs, len)) => {
                    flush(&mut text, &style, &mut spans);
                    let counter = match name.as_str() {
                        "b" => Some(&mut style.bold),
                        "i" => Some(&mut style.italic),
                        "u" => Some(&mut style.underline),
                        _ => None,
                    };
                    match (counter, name.as_str()) {
                        (Some(n), _) if closing => *n = n.saturating_sub(1),
                        (Some(n), _) => *n += 1,
                        (None, "a") if closing => {
                            style.link.pop();
                        }
                        (None, "a") => style.link.push(href(&attrs)),
                        _ => {}
                    }
                    i += len;
                }
                None => {
                    text.push('<');
                    i += 1;
                }
            },
            '&' => match entity(&body[i..]) {
                Some((ch, len)) => {
                    text.push(ch);
                    i += len;
                }
                None => {
                    text.push('&');
                    i += 1;
                }
            },
            _ => {
                text.push(c);
                i += c.len_utf8();
            }
        }
    }
    flush(&mut text, &style, &mut spans);
    spans
}

/// The plain text of a markup body.
pub fn plain(body: &str) -> String {
    parse(body).into_iter().map(|s| s.text).collect()
}

/// A plain-text body as one span.
pub fn text(body: &str) -> Vec<Span> {
    vec![Span { text: body.to_owned(), ..Default::default() }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_and_links() {
        let spans = parse("Hi <b>there</b>, see <a href=\"https://example.org/?a=1&amp;b=2\">this</a> &amp; <u>that</u>");
        let texts: Vec<&str> = spans.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["Hi ", "there", ", see ", "this", " & ", "that"]);
        assert!(spans[1].bold && !spans[0].bold);
        assert_eq!(spans[3].link.as_deref(), Some("https://example.org/?a=1&b=2"));
        assert!(spans[5].underline);
    }

    #[test]
    fn unknown_tags_and_stray_brackets() {
        assert_eq!(plain("<img src=\"x.png\" alt=\"pic\"/>a < b &copy; &#65;&#x42;"), "a < b &copy; AB");
        assert_eq!(plain("<i>tilted</i> <span>kept</span>"), "tilted kept");
        assert_eq!(plain("1 <2 and 3> 0"), "1 <2 and 3> 0");
    }
}
