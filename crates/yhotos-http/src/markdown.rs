//! Bounded Markdown subset; no raw HTML, images, scripts, or unsafe URL schemes.
pub(crate) fn render(source: &str) -> String {
    let source = source
        .get(..source.floor_char_boundary(source.len().min(32768)))
        .unwrap_or("");
    let parser = pulldown_cmark::Parser::new(source).filter(|event| {
        !matches!(
            event,
            pulldown_cmark::Event::Html(_) | pulldown_cmark::Event::InlineHtml(_)
        )
    });
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    ammonia::Builder::default()
        .tags(
            [
                "p",
                "br",
                "em",
                "strong",
                "ul",
                "ol",
                "li",
                "blockquote",
                "code",
                "pre",
                "a",
            ]
            .into_iter()
            .collect(),
        )
        .url_schemes(["https"].into_iter().collect())
        .clean(&html)
        .to_string()
}
