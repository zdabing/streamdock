pub mod cat_catch;
pub mod container;
pub mod contract;
pub mod crypto;
pub mod download;
pub mod error;
pub mod extractor;
pub mod file_download;
pub mod format_options;
pub mod http;
pub mod m3u8_download;
pub mod m3u8_parse;
pub mod merge;
pub mod rss_filter;
pub mod rss_parser;
pub mod rss_service;
pub mod scraper;
pub mod signed_hls_proxy;
pub mod store;
pub mod video_parser;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
