use crate::contract::{RssItemStatus, RssRejectReason};
use regex::{Regex, RegexBuilder};
use std::collections::HashSet;
use std::sync::LazyLock;
use unicode_normalization::UnicodeNormalization;

#[derive(Debug, Clone, Default)]
pub struct RssFilterRule {
    pub include_pattern: String,
    pub exclude_pattern: String,
    pub use_regex: bool,
    pub smart_episode: bool,
    pub size_min_bytes: u64,
    pub size_max_bytes: u64,
}

#[derive(Debug, Clone)]
enum CompiledMatcher {
    Any,
    Regex(Regex),
    Keywords(Vec<Vec<String>>),
}

#[derive(Debug, Clone)]
pub struct CompiledRssFilter {
    include: CompiledMatcher,
    exclude: CompiledMatcher,
    pub smart_episode: bool,
    pub size_min_bytes: u64,
    pub size_max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RssFilterVerdict {
    Accepted {
        episode_key: String,
    },
    Rejected {
        status: RssItemStatus,
        reason: RssRejectReason,
        episode_key: String,
    },
}

fn is_common_resolution(n: u32) -> bool {
    matches!(n, 360 | 480 | 720 | 1080 | 1440 | 2160 | 4320)
}

fn build_matcher(expr: &str, use_regex: bool) -> CompiledMatcher {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return CompiledMatcher::Any;
    }
    if use_regex {
        match RegexBuilder::new(trimmed).case_insensitive(true).build() {
            Ok(rx) => CompiledMatcher::Regex(rx),
            Err(_) => CompiledMatcher::Any,
        }
    } else {
        let alts: Vec<Vec<String>> = trimmed
            .split('|')
            .map(|alt| {
                alt.trim()
                    .to_lowercase()
                    .split_whitespace()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|words| !words.is_empty())
            .collect();
        if alts.is_empty() {
            CompiledMatcher::Any
        } else {
            CompiledMatcher::Keywords(alts)
        }
    }
}

fn matcher_matches(matcher: &CompiledMatcher, title: &str, lowered: &str) -> bool {
    match matcher {
        CompiledMatcher::Any => true,
        CompiledMatcher::Regex(rx) => rx.is_match(title),
        CompiledMatcher::Keywords(alts) => alts
            .iter()
            .any(|words| words.iter().all(|w| lowered.contains(w.as_str()))),
    }
}

fn is_digit_or_x(ch: char) -> bool {
    ch.is_ascii_digit() || ch == 'x' || ch == 'X'
}

pub fn extract_episode_key(title: &str) -> String {
    let text: String = title.nfkc().collect();
    if text.is_empty() {
        return String::new();
    }

    static SXE_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)\bS(\d{1,2})\s*[.\-_ ]?\s*E(\d{1,4})(?:v\d+)?\b").unwrap()
    });
    if let Some(caps) = SXE_RX.captures(&text) {
        if let (Ok(s), Ok(e)) = (caps[1].parse::<u32>(), caps[2].parse::<u32>()) {
            if s <= 99 && e <= 999 {
                return format!("S{s:02}E{e:02}");
            }
        }
    }

    static NXM_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(\d{1,2})[xX](\d{1,3})").unwrap());
    for caps in NXM_RX.captures_iter(&text) {
        let m = caps.get(0).unwrap();
        let before_ok = text[..m.start()]
            .chars()
            .next_back()
            .is_none_or(|c| !is_digit_or_x(c));
        let after_ok = text[m.end()..]
            .chars()
            .next()
            .is_none_or(|c| !is_digit_or_x(c));
        if before_ok && after_ok {
            if let (Ok(s), Ok(e)) = (caps[1].parse::<u32>(), caps[2].parse::<u32>()) {
                if (1..=99).contains(&s) && e <= 999 {
                    return format!("S{s:02}E{e:02}");
                }
            }
        }
    }

    static CN_EP_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"第\s*(\d{1,4})\s*[集话話期回]").unwrap());
    if let Some(caps) = CN_EP_RX.captures(&text) {
        if let Ok(e) = caps[1].parse::<u32>() {
            return format!("E{e:02}");
        }
    }

    static EP_TAG_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?i)\bEP?\s*(\d{1,4})(?:v\d+)?\b").unwrap());
    if let Some(caps) = EP_TAG_RX.captures(&text) {
        if let Ok(e) = caps[1].parse::<u32>() {
            if e <= 999 {
                return format!("E{e:02}");
            }
        }
    }

    static BRACKET_EP_RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)(?:\[|【)\s*0*(\d{1,4})(?:v\d+)?(?:\s*(?:END|Fin|完))?\s*(?:\]|】)")
            .unwrap()
    });
    for caps in BRACKET_EP_RX.captures_iter(&text) {
        if let Ok(num) = caps[1].parse::<u32>() {
            if num <= 999 && !is_common_resolution(num) {
                return format!("E{num:02}");
            }
        }
    }

    static DASH_EP_RX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\s-\s*0*(\d{1,3})(?:v\d+)?(?:\s*(?:\[|【|\()|$)").unwrap());
    if let Some(caps) = DASH_EP_RX.captures(&text) {
        if let Ok(num) = caps[1].parse::<u32>() {
            if num <= 999 && !is_common_resolution(num) {
                return format!("E{num:02}");
            }
        }
    }

    String::new()
}

pub fn compile_rss_filter(rule: &RssFilterRule) -> CompiledRssFilter {
    CompiledRssFilter {
        include: build_matcher(&rule.include_pattern, rule.use_regex),
        exclude: build_matcher(&rule.exclude_pattern, rule.use_regex),
        smart_episode: rule.smart_episode,
        size_min_bytes: rule.size_min_bytes,
        size_max_bytes: rule.size_max_bytes,
    }
}

pub fn evaluate_rss_filter(
    compiled: &CompiledRssFilter,
    title: &str,
    enclosure_length: u64,
    mut seen_episodes: Option<&mut HashSet<String>>,
) -> RssFilterVerdict {
    let lowered = title.to_lowercase();
    if !matcher_matches(&compiled.include, title, &lowered) {
        return RssFilterVerdict::Rejected {
            status: RssItemStatus::Filtered,
            reason: RssRejectReason::NotIncluded,
            episode_key: String::new(),
        };
    }
    if !matches!(compiled.exclude, CompiledMatcher::Any)
        && matcher_matches(&compiled.exclude, title, &lowered)
    {
        return RssFilterVerdict::Rejected {
            status: RssItemStatus::Filtered,
            reason: RssRejectReason::Excluded,
            episode_key: String::new(),
        };
    }

    if enclosure_length > 0 {
        if compiled.size_min_bytes > 0 && enclosure_length < compiled.size_min_bytes {
            return RssFilterVerdict::Rejected {
                status: RssItemStatus::Filtered,
                reason: RssRejectReason::TooSmall,
                episode_key: String::new(),
            };
        }
        if compiled.size_max_bytes > 0 && enclosure_length > compiled.size_max_bytes {
            return RssFilterVerdict::Rejected {
                status: RssItemStatus::Filtered,
                reason: RssRejectReason::TooLarge,
                episode_key: String::new(),
            };
        }
    }

    let episode_key = if compiled.smart_episode {
        extract_episode_key(title)
    } else {
        String::new()
    };
    if compiled.smart_episode && !episode_key.is_empty() {
        if let Some(ref mut seen) = seen_episodes {
            if seen.contains(&episode_key) {
                return RssFilterVerdict::Rejected {
                    status: RssItemStatus::DuplicateEpisode,
                    reason: RssRejectReason::DupEpisode,
                    episode_key,
                };
            }
            seen.insert(episode_key.clone());
        }
    }

    RssFilterVerdict::Accepted { episode_key }
}

pub fn parse_human_size(raw: &str) -> Option<u64> {
    let text = raw.trim();
    if text.is_empty() {
        return Some(0);
    }
    static RX: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(\d+(?:\.\d+)?)\s*(B|K|KB|KiB|M|MB|MiB|G|GB|GiB|T|TB|TiB)?$").unwrap()
    });
    let caps = RX.captures(text)?;
    let value: f64 = caps[1].parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let unit = caps
        .get(2)
        .map(|m| m.as_str().to_ascii_uppercase())
        .unwrap_or_else(|| "B".to_string());
    let power: u32 = if unit.starts_with('K') {
        1
    } else if unit.starts_with('M') {
        2
    } else if unit.starts_with('G') {
        3
    } else if unit.starts_with('T') {
        4
    } else {
        0
    };
    let base: f64 = if unit.ends_with("IB") || power == 0 {
        1024.0
    } else {
        1000.0
    };
    Some((value * base.powi(power as i32)) as u64)
}
