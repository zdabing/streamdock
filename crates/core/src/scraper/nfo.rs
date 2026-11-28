use crate::scraper::detail::DetailMetadata;

pub fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        let code = ch as u32;
        if code < 0x20 && code != 0x09 && code != 0x0a && code != 0x0d {
            continue;
        }
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

#[derive(Debug, Clone, Default)]
pub struct NfoArtworkNames {
    pub poster: String,
    pub fanart: String,
    pub thumb: String,
}

pub fn render_emby_nfo(meta: &DetailMetadata, artwork: &NfoArtworkNames) -> String {
    let mut lines = vec![
        "<?xml version=\"1.0\" encoding=\"utf-8\" standalone=\"yes\"?>".to_string(),
        "<movie>".to_string(),
    ];

    let push_tag = |lines: &mut Vec<String>, tag: &str, val: &str| {
        let text = val.trim();
        if text.is_empty() {
            return;
        }
        lines.push(format!("  <{tag}>{}</{tag}>", escape_xml(text)));
    };

    push_tag(&mut lines, "title", &meta.title);
    push_tag(&mut lines, "originaltitle", &meta.title);
    push_tag(&mut lines, "sorttitle", &meta.title);
    push_tag(&mut lines, "plot", &meta.plot);

    let outline = if meta.plot.chars().count() > 200 {
        let prefix: String = meta.plot.chars().take(197).collect();
        format!("{prefix}...")
    } else {
        meta.plot.clone()
    };
    push_tag(&mut lines, "outline", &outline);

    if !meta.premiered.is_empty() {
        push_tag(&mut lines, "premiered", &meta.premiered);
        push_tag(&mut lines, "releasedate", &meta.premiered);
    }
    if !meta.year.is_empty() {
        push_tag(&mut lines, "year", &meta.year);
    }
    if meta.runtime > 0 {
        push_tag(&mut lines, "runtime", &meta.runtime.to_string());
    }
    push_tag(&mut lines, "studio", &meta.studio);
    push_tag(&mut lines, "maker", &meta.studio);
    push_tag(&mut lines, "publisher", &meta.studio);
    if !meta.unique_id.is_empty() {
        push_tag(&mut lines, "num", &meta.unique_id);
        let utype = if meta.unique_id_type.is_empty() {
            "rss"
        } else {
            &meta.unique_id_type
        };
        lines.push(format!(
            "  <uniqueid type=\"{}\" default=\"true\">{}</uniqueid>",
            escape_xml(utype),
            escape_xml(&meta.unique_id)
        ));
    }
    push_tag(&mut lines, "website", &meta.page_url);

    for actor in &meta.actors {
        let clean = actor.trim();
        if clean.is_empty() {
            continue;
        }
        lines.push("  <actor>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(clean)));
        lines.push("    <type>Actor</type>".to_string());
        lines.push("  </actor>".to_string());
    }

    for genre in &meta.genres {
        push_tag(&mut lines, "genre", genre);
        push_tag(&mut lines, "tag", genre);
    }

    if !artwork.poster.is_empty() {
        push_tag(&mut lines, "poster", &artwork.poster);
        lines.push(format!(
            "  <thumb aspect=\"poster\">{}</thumb>",
            escape_xml(&artwork.poster)
        ));
    }
    if !artwork.fanart.is_empty() {
        lines.push("  <fanart>".to_string());
        lines.push(format!(
            "    <thumb>{}</thumb>",
            escape_xml(&artwork.fanart)
        ));
        lines.push("  </fanart>".to_string());
    } else if !artwork.thumb.is_empty() {
        push_tag(&mut lines, "thumb", &artwork.thumb);
    }

    lines.push("</movie>".to_string());
    lines.push(String::new());
    lines.join("\n")
}
