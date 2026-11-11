//! Shared layout and small UI components (maud + Tailwind + daisyUI).
use crate::auth::User;
use crate::ops::{Label, Person};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use maud::{html, Markup, PreEscaped, DOCTYPE};
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;

const ASSETS: &[(&str, &str, &str)] = &[
    ("app.css", "text/css; charset=utf-8", include_str!("../static/app.css")),
    ("app.js", "text/javascript; charset=utf-8", include_str!("../static/app.js")),
    ("htmx.min.js", "text/javascript; charset=utf-8", include_str!("../static/vendor/htmx.min.js")),
    ("Sortable.min.js", "text/javascript; charset=utf-8", include_str!("../static/vendor/Sortable.min.js")),
    ("favicon.svg", "image/svg+xml", include_str!("../static/favicon.svg")),
];

/// Content hash so assets can be cached forever and still update on deploy.
static ASSET_VERSION: LazyLock<String> = LazyLock::new(|| {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (_, _, body) in ASSETS {
        body.hash(&mut h);
    }
    format!("{:x}", h.finish())
});

fn asset(name: &str) -> String {
    format!("/static/{name}?v={}", *ASSET_VERSION)
}

pub async fn serve_asset(axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    match ASSETS.iter().find(|(n, _, _)| *n == name) {
        Some((_, mime, body)) => (
            [(header::CONTENT_TYPE, *mime), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")],
            *body,
        )
            .into_response(),
        None => axum::http::StatusCode::NOT_FOUND.into_response(),
    }
}

/// Every theme that ships with daisyUI 5.
pub const THEMES: &[&str] = &[
    "light", "dark", "cupcake", "bumblebee", "emerald", "corporate", "synthwave", "retro", "cyberpunk", "valentine",
    "halloween", "garden", "forest", "aqua", "lofi", "pastel", "fantasy", "wireframe", "black", "luxury", "dracula",
    "cmyk", "autumn", "business", "acid", "lemonade", "night", "coffee", "winter", "dim", "nord", "sunset",
    "caramellatte", "abyss", "silk",
];

/// The Autokan mark: an automation gear with a lightning bolt struck through it.
/// The gear takes the theme's primary colour, the bolt its warning colour.
pub fn logo(class: &str) -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32" class={ "logo text-primary shrink-0 " (class) } aria-hidden="true" {
            g fill="currentColor" {
                @for i in 0..8 {
                    rect x="13.5" y="1.5" width="5" height="7" rx="1.2" transform={ "rotate(" (i * 45) " 16 16)" } {}
                }
            }
            circle cx="16" cy="16" r="8.5" fill="none" stroke="currentColor" stroke-width="5" {}
            path class="logo-bolt" d="M19 1.5 8 17.5h6.5L12 30.5l12-17h-6.8z" stroke-width="1.6" stroke-linejoin="round" {}
        }
    }
}

/// Small icons that tell the three levels of the hierarchy apart: "team", "project" (a folder)
/// and "board" (kanban columns).
pub fn icon(kind: &str, class: &str) -> Markup {
    html! {
        svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" class={ "shrink-0 " (class) } fill="currentColor" aria-hidden="true" {
            @match kind {
                "project" => { path d="M3 7a2 2 0 0 1 2-2h4.2a2 2 0 0 1 1.4.6L12 7h7a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z" {} },
                "board" => {
                    rect x="3" y="4" width="5" height="16" rx="1.5" {}
                    rect x="9.5" y="4" width="5" height="10" rx="1.5" {}
                    rect x="16" y="4" width="5" height="13" rx="1.5" {}
                },
                _ => {
                    circle cx="8" cy="9" r="3" {}
                    circle cx="16.5" cy="10" r="2.5" {}
                    path d="M2 19a6 6 0 0 1 12 0v1H2zM15.5 20v-1a7.5 7.5 0 0 0-1.2-4 5 5 0 0 1 7.7 4v1z" {}
                },
            }
        }
    }
}

/// Tiny uppercase tag naming what kind of thing a tile is.
pub fn kind_tag(text: &str) -> Markup {
    html! { span class="text-[10px] font-bold uppercase tracking-widest opacity-50" { (text) } }
}

fn theme_switcher() -> Markup {
    let swatch = |theme: Option<&str>| {
        html! {
            span data-theme=[theme] class="grid grid-cols-4 gap-0.5 rounded-md p-1 bg-base-100 border border-base-content/20 shrink-0" {
                span class="w-1.5 h-4 rounded-sm bg-primary" {}
                span class="w-1.5 h-4 rounded-sm bg-secondary" {}
                span class="w-1.5 h-4 rounded-sm bg-accent" {}
                span class="w-1.5 h-4 rounded-sm bg-neutral" {}
            }
        }
    };
    let option = |value: &str, label: &str, theme: Option<&str>| {
        html! {
            button type="button" data-set-theme=(value)
                class="flex items-center gap-3 rounded-lg px-2 py-1.5 text-left cursor-pointer hover:bg-base-200 transition-colors" {
                (swatch(theme))
                span class="flex-1 text-sm capitalize" { (label) }
                span class="theme-check invisible text-primary font-bold" { "✓" }
            }
        }
    };
    html! {
        div class="dropdown dropdown-end" {
            div tabindex="0" role="button" class="btn btn-ghost btn-sm gap-2" title="Change theme" aria-label="Change theme" {
                svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" class="w-4 h-4" fill="currentColor" {
                    path d="M12 2a10 10 0 1 0 0 20c1.1 0 2-.9 2-2 0-.5-.2-1-.5-1.3-.3-.4-.5-.8-.5-1.3 0-1.1.9-2 2-2h2.3A4.7 4.7 0 0 0 22 10.7C22 5.9 17.5 2 12 2ZM6.5 12a1.5 1.5 0 1 1 0-3 1.5 1.5 0 0 1 0 3Zm3-4a1.5 1.5 0 1 1 0-3 1.5 1.5 0 0 1 0 3Zm5 0a1.5 1.5 0 1 1 0-3 1.5 1.5 0 0 1 0 3Zm3 4a1.5 1.5 0 1 1 0-3 1.5 1.5 0 0 1 0 3Z" {}
                }
                span class="hidden sm:inline" { "Theme" }
            }
            // No display utility on .dropdown-content itself: daisyUI hides the closed menu with its own
            // display rule, and overriding it leaves an invisible, clickable menu over the page.
            div tabindex="0" class="dropdown-content bg-base-100 text-base-content rounded-box z-40 w-60 mt-2 p-2 shadow-2xl border border-base-300 max-h-[70vh] overflow-y-auto" {
                div class="flex flex-col gap-0.5" {
                    (option("", "Match system", None))
                    div class="divider my-0" {}
                    @for t in THEMES { (option(t, t, Some(t))) }
                }
            }
        }
    }
}

/// `full` pages (the board) fill the viewport and manage their own scrolling.
pub fn page(user: Option<&User>, title: &str, full: bool, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Autokan" }
                link rel="icon" type="image/svg+xml" href=(asset("favicon.svg"));
                script { (PreEscaped("document.documentElement.dataset.theme = localStorage.getItem('theme') || (matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light');")) }
                link rel="stylesheet" href=(asset("app.css"));
                script defer src=(asset("htmx.min.js")) {}
                script defer src=(asset("Sortable.min.js")) {}
                script defer src=(asset("app.js")) {}
            }
            body class={ "app-bg bg-base-200 text-base-content flex flex-col " (if full { "h-screen overflow-hidden" } else { "min-h-screen" }) } {
                div class="navbar sticky top-0 z-30 bg-base-100/80 backdrop-blur-md border-b border-base-300 shadow-sm min-h-14 px-4 gap-1 shrink-0" {
                    a class="brand flex items-center gap-2 mr-2" href="/" {
                        (logo("w-8 h-8"))
                        span class="brand-text text-xl font-extrabold tracking-tight" { "Autokan" }
                    }
                    div class="flex-1" {}
                    (theme_switcher())
                    @if let Some(u) = user {
                        @if u.sees_admin() {
                            a class="btn btn-ghost btn-sm" href="/admin" { "Admin" }
                        }
                        div class="dropdown dropdown-end" {
                            div tabindex="0" role="button" class="btn btn-ghost btn-sm gap-2 pl-1" {
                                span class="inline-flex items-center justify-center w-7 h-7 rounded-full bg-primary text-primary-content text-xs font-bold" {
                                    (initials(&u.display_name))
                                }
                                span class="hidden sm:inline" { (u.display_name) }
                            }
                            ul tabindex="0" class="dropdown-content menu bg-base-100 rounded-box z-40 w-56 mt-2 p-2 shadow-2xl border border-base-300" {
                                li class="menu-title" {
                                    (u.username)
                                    @if u.is_breakglass() { " · break glass" }
                                }
                                li {
                                    form method="post" action="/logout" class="p-0" {
                                        button class="w-full text-left px-3 py-1 cursor-pointer" { "Sign out" }
                                    }
                                }
                            }
                        }
                    }
                }
                main class=(if full { "flex-1 min-h-0" } else { "container mx-auto max-w-6xl p-4 md:p-6 flex-1 flex flex-col gap-5" }) {
                    (body)
                }
                div id="toasts" class="toast toast-end z-50" {}
            }
        }
    }
}

/// A titled panel.
pub fn panel(title: &str, body: Markup) -> Markup {
    html! {
        section class="card bg-base-100 border border-base-300 shadow-sm" {
            div class="card-body p-5 gap-3" {
                h2 class="card-title text-base" { (title) }
                (body)
            }
        }
    }
}

pub fn crumbs(items: &[(&str, Option<String>)]) -> Markup {
    html! {
        div class="breadcrumbs text-sm py-0" {
            ul {
                @for (name, href) in items {
                    li {
                        @if let Some(h) = href { a href=(h) { (name) } } @else { span class="font-semibold" { (name) } }
                    }
                }
            }
        }
    }
}

pub const LABEL_COLORS: &[(&str, &str)] = &[
    ("red", "bg-red-500 text-white"),
    ("orange", "bg-orange-500 text-white"),
    ("yellow", "bg-yellow-400 text-black"),
    ("green", "bg-green-600 text-white"),
    ("teal", "bg-teal-500 text-white"),
    ("blue", "bg-blue-600 text-white"),
    ("purple", "bg-purple-600 text-white"),
    ("pink", "bg-pink-500 text-white"),
    ("gray", "bg-gray-500 text-white"),
];

pub fn label_class(color: &str) -> &'static str {
    LABEL_COLORS
        .iter()
        .find(|(c, _)| *c == color)
        .map(|(_, cls)| *cls)
        .unwrap_or("bg-gray-500 text-white")
}

pub fn label_badge(l: &Label) -> Markup {
    html! { span class={ "badge badge-sm border-0 " (label_class(&l.color)) } { (l.name) } }
}

pub fn initials(name: &str) -> String {
    let mut out: String = name
        .split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .collect::<String>()
        .to_uppercase();
    if out.is_empty() {
        out.push('?');
    }
    out
}

pub fn avatar(p: &Person) -> Markup {
    html! {
        span class="inline-flex items-center justify-center w-6 h-6 rounded-full bg-neutral text-neutral-content text-[10px] font-semibold"
            title=(p.display_name) { (initials(&p.display_name)) }
    }
}

/// UTC timestamp that app.js rewrites into the viewer's local time.
pub fn timestamp(utc: &str) -> Markup {
    html! { time datetime={ (utc.replace(' ', "T")) "Z" } data-local { (utc) " UTC" } }
}

/// Markdown textarea with image / video upload (button, paste or drag and drop).
pub fn editor(board_id: i64, name: &str, value: &str, placeholder: &str, rows: u8) -> Markup {
    html! {
        div class="editor flex flex-col gap-1" data-upload-url={ "/boards/" (board_id) "/uploads" } {
            textarea class="textarea w-full font-mono text-sm" name=(name) rows=(rows) placeholder=(placeholder) { (value) }
            div class="flex items-center gap-2" {
                label class="btn btn-xs" {
                    "Attach file"
                    input type="file" class="hidden" multiple;
                }
                span class="text-xs opacity-60" data-upload-status { "Markdown. Paste or drop images, videos or documents to upload." }
            }
        }
    }
}

/// Service / model picker. The empty option means "inherit" (its wording is the caller's).
/// Values are `"<service id>|<model>"`, parsed by `llm::parse_target`.
pub fn target_select(name: &str, services: &[crate::llm::Service], current: &str, inherit_label: &str) -> Markup {
    html! {
        select class="select w-full" name=(name) {
            option value="" selected[current.is_empty()] { (inherit_label) }
            @for s in services.iter().filter(|s| s.enabled || current.starts_with(&format!("{}|", s.id))) {
                optgroup label={ (s.name) " — " (s.kind_label()) } {
                    @for m in s.model_list() {
                        @let value = format!("{}|{}", s.id, m);
                        option value=(value) selected[value == current] { (s.name) " · " (m) }
                    }
                }
            }
        }
    }
}
