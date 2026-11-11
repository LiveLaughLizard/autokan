//! Markdown -> sanitized HTML. Image syntax pointing at a video file renders a <video> player.
use maud::{html, Markup, PreEscaped};
use pulldown_cmark::{html::push_html, Event, Options, Parser, Tag, TagEnd};
use std::sync::LazyLock;

pub const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "avif"];
pub const VIDEO_EXTS: &[&str] = &["mp4", "webm", "mov", "m4v", "ogv"];

static SANITIZER: LazyLock<ammonia::Builder<'static>> = LazyLock::new(|| {
    let mut b = ammonia::Builder::default();
    b.add_tags(["video"])
        .add_tag_attributes("video", ["src", "controls", "preload"]);
    b
});

fn is_video(url: &str) -> bool {
    let path = url.split(['?', '#']).next().unwrap_or("");
    path.rsplit_once('.')
        .is_some_and(|(_, ext)| VIDEO_EXTS.contains(&ext.to_ascii_lowercase().as_str()))
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn to_html(src: &str) -> String {
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let mut in_video = false;
    let events = Parser::new_ext(src, opts).filter_map(|ev| match ev {
        Event::Start(Tag::Image { ref dest_url, .. }) if is_video(dest_url) => {
            in_video = true;
            Some(Event::Html(
                format!("<video controls preload=\"metadata\" src=\"{}\"></video>", escape_attr(dest_url)).into(),
            ))
        }
        Event::End(TagEnd::Image) if in_video => {
            in_video = false;
            None
        }
        _ if in_video => None,
        other => Some(other),
    });
    let mut out = String::new();
    push_html(&mut out, events);
    SANITIZER.clean(&out).to_string()
}

pub fn render(src: &str) -> Markup {
    html! {
        div class="prose prose-sm max-w-none break-words" { (PreEscaped(to_html(src))) }
    }
}
