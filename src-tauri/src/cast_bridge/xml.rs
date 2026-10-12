// Just enough XML for UPnP: device descriptions and SOAP replies are flat and machine-written,
// so elements are looked up by local name rather than through a full parser.

/// Inner text of the first element with this local name at or after `from`, plus where it ended.
fn find_element<'a>(xml: &'a str, name: &str, from: usize) -> Option<(&'a str, usize)> {
    let mut cursor = from;
    while let Some(offset) = xml.get(cursor..)?.find('<') {
        let open = cursor + offset;
        let rest = &xml[open + 1..];
        cursor = open + 1;
        let name_end = rest.find(|character: char| character.is_whitespace() || character == '>' || character == '/').unwrap_or(rest.len());
        let qualified = &rest[..name_end];
        let local = qualified.rsplit(':').next().unwrap_or(qualified);
        if qualified.is_empty() || local != name {
            continue;
        }
        let tag_close = rest.find('>')?;
        let after_tag = open + 1 + tag_close + 1;
        if rest[..tag_close].ends_with('/') {
            return Some(("", after_tag));
        }
        let closing = format!("</{qualified}>");
        let close_offset = xml[after_tag..].find(&closing)?;
        return Some((&xml[after_tag..after_tag + close_offset], after_tag + close_offset + closing.len()));
    }
    None
}

fn strip_cdata(text: &str) -> &str {
    text.strip_prefix("<![CDATA[").and_then(|inner| inner.strip_suffix("]]>")).unwrap_or(text)
}

/// Trimmed, unescaped text of the first element with this local name.
pub fn tag_text(xml: &str, name: &str) -> Option<String> {
    let (inner, _) = find_element(xml, name, 0)?;
    let text = unescape(strip_cdata(inner.trim()));
    (!text.is_empty()).then_some(text)
}

/// Raw inner content of every element with this local name, in document order.
pub fn tag_blocks<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let mut blocks = Vec::new();
    let mut cursor = 0;
    while let Some((inner, next)) = find_element(xml, name, cursor) {
        blocks.push(inner);
        cursor = next;
    }
    blocks
}

pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len() + 16);
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

pub fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    // `&amp;` last, so an escaped entity such as `&amp;lt;` comes out as the literal `&lt;`.
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_text_finds_plain_and_prefixed_elements() {
        let xml = r#"<root xmlns:u="urn:x"><friendlyName> Living Room </friendlyName><u:RelTime>0:01:02</u:RelTime></root>"#;
        assert_eq!(tag_text(xml, "friendlyName").as_deref(), Some("Living Room"));
        assert_eq!(tag_text(xml, "RelTime").as_deref(), Some("0:01:02"));
        assert_eq!(tag_text(xml, "missing"), None);
    }

    #[test]
    fn tag_text_does_not_match_a_longer_name_or_a_closing_tag() {
        let xml = "<a><modelNameExtra>x</modelNameExtra><modelName>TV</modelName></a>";
        assert_eq!(tag_text(xml, "modelName").as_deref(), Some("TV"));
    }

    #[test]
    fn tag_text_unescapes_entities_and_cdata() {
        assert_eq!(tag_text("<n>Tom &amp; Jerry&apos;s</n>", "n").as_deref(), Some("Tom & Jerry's"));
        assert_eq!(tag_text("<n><![CDATA[A <b> C]]></n>", "n").as_deref(), Some("A <b> C"));
    }

    #[test]
    fn tag_text_handles_attributes_and_empty_elements() {
        assert_eq!(tag_text(r#"<CurrentVolume channel="Master">37</CurrentVolume>"#, "CurrentVolume").as_deref(), Some("37"));
        assert_eq!(tag_text("<a><RelTime/></a>", "RelTime"), None);
    }

    #[test]
    fn tag_blocks_returns_each_service() {
        let xml = "<serviceList><service><serviceType>A</serviceType></service><service><serviceType>B</serviceType></service></serviceList>";
        let blocks = tag_blocks(xml, "service");
        assert_eq!(blocks.len(), 2);
        assert_eq!(tag_text(blocks[1], "serviceType").as_deref(), Some("B"));
    }

    #[test]
    fn escape_and_unescape_round_trip() {
        let original = r#"http://h/x?a=1&b=<2> "q" 'p'"#;
        let escaped = escape(original);
        assert!(!escaped.contains('<') && !escaped.contains('"'));
        assert_eq!(unescape(&escaped), original);
        assert_eq!(unescape(&escape(&escaped)), escaped);
    }

    #[test]
    fn malformed_input_does_not_panic() {
        assert_eq!(tag_text("<a><b>unterminated", "b"), None);
        assert_eq!(tag_text("<", "b"), None);
        assert!(tag_blocks("<<<>>>", "b").is_empty());
    }
}
