use crate::contract::{ApiResponse, HeaderMap, VideoInfo};
use crate::error::{CoreError, CoreResult};
use crate::http::normalize_download_url;
use crate::video_parser::request::{PageRequest, ParserContext, locate, read_page};
use regex::Regex;
use std::sync::LazyLock;
use url::Url;

pub async fn parse_pipixia(
    url: &str,
    headers: &HeaderMap,
    ctx: &ParserContext,
) -> CoreResult<ApiResponse<VideoInfo>> {
    let mut outgoing = HeaderMap::new();
    outgoing.insert("referer".to_string(), "https://www.pipix.com/".to_string());
    for (k, v) in headers {
        outgoing.insert(k.clone(), v.clone());
    }

    let res: CoreResult<VideoInfo> = async {
        let location = locate(
            ctx,
            url,
            &PageRequest {
                headers: outgoing.clone(),
                ..Default::default()
            },
        )
        .await?;
        let parsed = Url::parse(&location)?;
        static ID_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/item/(\d+)").unwrap());
        let id = ID_RX
            .captures(parsed.path())
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .ok_or_else(|| CoreError::msg("无法解析皮皮虾视频 ID，请检查分享链接"))?;

        let api_url = format!(
            "https://h5.pipix.com/bds/cell/cell_h5_comment/?cell_id={id}&aid=1319&app_name=super"
        );
        let page = read_page(
            ctx,
            &api_url,
            &PageRequest {
                headers: outgoing,
                ..Default::default()
            },
        )
        .await?;
        let json: serde_json::Value = serde_json::from_str(&page.text)?;
        let item = if json["data"]["cell_comments"][0]["comment_info"]["item"].is_object() {
            &json["data"]["cell_comments"][0]["comment_info"]["item"]
        } else if json["data"]["item"].is_object() {
            &json["data"]["item"]
        } else {
            &json["data"]["cell_data"]["item"]
        };

        let video = &item["video"];
        let variant = if video["video_high"].is_object() {
            &video["video_high"]
        } else if video["video_mid"].is_object() {
            &video["video_mid"]
        } else {
            &video["video_low"]
        };
        let addr = &variant["url_list"][0];
        let media = addr.as_str().or_else(|| addr["url"].as_str()).unwrap_or("");
        if media.is_empty() {
            return Err(CoreError::msg(
                "皮皮虾未返回视频数据，内容可能已失效、需要 Cookie 或接口已变化",
            ));
        }

        Ok(VideoInfo {
            url: normalize_download_url(media)?,
            title: item["content"].as_str().unwrap_or("").to_string(),
            platform: Some("pipixia".to_string()),
            referer: Some(location),
            author: item["author"]["name"].as_str().map(str::to_string),
            uid: item["author"]["uid"]
                .as_i64()
                .map(|n| n.to_string())
                .or_else(|| item["author"]["uid"].as_str().map(str::to_string)),
            like: item["digg_count"].as_i64(),
            time: item["create_time"].as_i64(),
            avatar: item["author"]["avatar"]["url_list"][0]["url"]
                .as_str()
                .map(str::to_string),
            cover: item["cover"]["url_list"][0]["url"]
                .as_str()
                .map(str::to_string),
            music: None,
        })
    }
    .await;

    match res {
        Ok(info) => Ok(ApiResponse::ok(info)),
        Err(e) if ctx.cancel.is_cancelled() || e.is_aborted() => Err(e),
        Err(e) => Ok(ApiResponse::err(1, format!("皮皮虾解析失败：{e}"))),
    }
}
