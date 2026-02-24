/// Render an email body to plain text for display.
///
/// Strategy:
/// 1. If text/plain is available, use it directly
/// 2. If only text/html, convert via html2text
pub fn render_body(text_plain: Option<&str>, text_html: Option<&str>) -> String {
    if let Some(plain) = text_plain {
        return plain.to_string();
    }

    if let Some(html) = text_html {
        return html_to_text(html);
    }

    "[No displayable content]".to_string()
}

/// Convert HTML email body to readable plain text.
fn html_to_text(html: &str) -> String {
    html2text::from_read(html.as_bytes(), 80).unwrap_or_default()
}

/// Render email body as Markdown for the preview widget.
/// Pipeline: HTML → ammonia (strip layout junk) → html2md convert.
/// Prefers HTML when available (produces richer output than plain text).
pub fn render_body_markdown(text_plain: Option<&str>, text_html: Option<&str>) -> String {
    if let Some(html) = text_html {
        let clean = clean_email_html(html);
        return html2md::parse_html(&clean);
    }
    if let Some(plain) = text_plain {
        return plain.to_string();
    }
    "[No displayable content]".to_string()
}

/// Strip email HTML down to semantic content only.
///
/// Marketing emails are 90% layout tables, tracking pixels, MSO conditionals,
/// and inline styles. ammonia::clean() default keeps all of that because it's
/// "safe" for browsers. But html2md faithfully converts every <table><tr><td>
/// into markdown table syntax, turning a 10-paragraph email into a monster.
///
/// We restrict ammonia to only semantic tags that html2md can meaningfully
/// convert. Text content inside stripped tags is preserved — only the tags
/// themselves are removed.
fn clean_email_html(html: &str) -> String {
    use std::collections::HashSet;
    let tags: HashSet<&str> = [
        // Block content
        "p", "br", "hr", "blockquote", "pre",
        // Headings
        "h1", "h2", "h3", "h4", "h5", "h6",
        // Inline formatting
        "b", "strong", "i", "em", "code", "s", "del", "u", "small", "sub", "sup",
        // Lists
        "ul", "ol", "li",
        // Links (ammonia will also sanitize href)
        "a",
    ]
    .iter()
    .copied()
    .collect();

    ammonia::Builder::new()
        .tags(tags)
        .clean(html)
        .to_string()
}

/// Open a URL in the system browser.
pub fn open_link(url: &str) {
    let _ = open::that(url);
}
