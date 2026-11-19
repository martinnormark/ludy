//! Icons, drawn for Ludy and compiled into the binary. gpui renders an SVG as
//! an alpha mask and colours it with the element's text colour.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

pub struct Assets;

macro_rules! icon {
    ($body:literal) => {
        concat!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round">"#,
            $body,
            "</svg>"
        )
    };
}

fn icon(name: &str) -> Option<&'static str> {
    Some(match name {
        "plus" => icon!(r#"<path d="M12 5.5v13M5.5 12h13"/>"#),
        "chevron-left" => icon!(r#"<path d="M14.5 6l-6 6 6 6"/>"#),
        "chevron-right" => icon!(r#"<path d="M9.5 6l6 6-6 6"/>"#),
        "refresh" => icon!(r#"<path d="M19.5 11A7.6 7.6 0 0 0 5.6 8M4.5 4.2V8h3.8M4.5 13a7.6 7.6 0 0 0 13.9 3M19.5 19.8V16h-3.8"/>"#),
        "grid" => icon!(r#"<rect x="4.5" y="4.5" width="6" height="6" rx="1.4"/><rect x="13.5" y="4.5" width="6" height="6" rx="1.4"/><rect x="4.5" y="13.5" width="6" height="6" rx="1.4"/><rect x="13.5" y="13.5" width="6" height="6" rx="1.4"/>"#),
        "list" => icon!(r#"<path d="M9 6.5h10.5M9 12h10.5M9 17.5h10.5M4.6 6.5h.1M4.6 12h.1M4.6 17.5h.1"/>"#),
        "import" => icon!(r#"<path d="M12 15.5V4.8M7.8 9L12 4.8 16.2 9M5 15.5v2.3a1.7 1.7 0 0 0 1.7 1.7h10.6a1.7 1.7 0 0 0 1.7-1.7v-2.3"/>"#),
        "folder-plus" => icon!(r#"<path d="M3.5 7.6a1.9 1.9 0 0 1 1.9-1.9h3.5l2 2.2h7.7a1.9 1.9 0 0 1 1.9 1.9v7a1.9 1.9 0 0 1-1.9 1.9H5.4a1.9 1.9 0 0 1-1.9-1.9zM12 11.2v4.6M9.7 13.5h4.6"/>"#),
        "eject" => icon!(r#"<path d="M12 5.8l6 7.4H6zM6 17.6h12"/>"#),
        "more" => icon!(r#"<path d="M6 12h.1M12 12h.1M18 12h.1" stroke-width="2.6"/>"#),
        "close" => icon!(r#"<path d="M6.5 6.5l11 11M17.5 6.5l-11 11"/>"#),
        "drive" => icon!(r#"<rect x="3.5" y="7" width="17" height="10" rx="2.6"/><path d="M7.2 12h.1" stroke-width="2.4"/><path d="M11.5 12h5.2"/>"#),
        "drives" => icon!(r#"<rect x="3.5" y="4.5" width="17" height="6.4" rx="2"/><rect x="3.5" y="13.1" width="17" height="6.4" rx="2"/><path d="M7 7.7h.1M7 16.3h.1" stroke-width="2.2"/>"#),
        "down" => icon!(r#"<path d="M12 5v13.5M6.8 13.3l5.2 5.2 5.2-5.2"/>"#),
        "up" => icon!(r#"<path d="M12 19V5.5M6.8 10.7L12 5.5l5.2 5.2"/>"#),
        "reveal" => icon!(r#"<path d="M13.5 5H19v5.5M19 5l-7.8 7.8M10.5 6.5H7A2 2 0 0 0 5 8.5V17a2 2 0 0 0 2 2h8.5a2 2 0 0 0 2-2v-3.5"/>"#),
        _ => return None,
    })
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let name = path.trim_start_matches("icons/").trim_end_matches(".svg");
        Ok(icon(name).map(|s| Cow::Borrowed(s.as_bytes())))
    }

    fn list(&self, _path: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(Vec::new())
    }
}
