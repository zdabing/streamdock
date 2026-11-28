use crate::contract::{ApiResponse, HeaderMap, VideoInfo};
use crate::error::{CoreError, CoreResult};
use crate::http::normalize_download_url;
use crate::video_parser::request::{PageRequest, ParserContext, locate, read_page};
use regex::Regex;
use std::sync::LazyLock;
use url::Url;

pub async fn parse_weibo(
    url: &str,
    headers: &HeaderMap,
    ctx: &ParserContext,
) -> CoreResult<ApiResponse<VideoInfo>> {
    let mut outgoing = HeaderMap::new();
    outgoing.insert("referer".to_string(), "https://weibo.com/".to_string());
    for (k, v) in headers {
        outgoing.insert(k.clone(), v.clone());
    }

    if !outgoing.get("cookie").is_some_and(|c| !c.trim().is_empty()) {
        return Ok(ApiResponse::err(
            1,
            "微博解析需要 Cookie，请在请求头中填写有效的微博 Cookie",
        ));
    }

    let res: CoreResult<VideoInfo> = async {
        let initial_parsed = Url::parse(url)?;
        let location = if initial_parsed
            .host_str()
            .is_some_and(|h| h.eq_ignore_ascii_case("t.cn"))
        {
            locate(
                ctx,
                url,
                &PageRequest {
                    headers: outgoing.clone(),
                    ..Default::default()
                },
            )
            .await?
        } else {
            url.to_string()
        };

        let parsed = Url::parse(&location)?;
        static OID_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d+:\d+").unwrap());
        static OID_FULL_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+:\d+$").unwrap());
        static NUM_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\d{3,4}").unwrap());
        static TAG_RX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());

        let oid = parsed
            .query_pairs()
            .find(|(k, _)| k == "fid")
            .map(|(_, v)| v.into_owned())
            .or_else(|| OID_RX.find(parsed.path()).map(|m| m.as_str().to_string()))
            .unwrap_or_default();

        let (item, media) = if OID_FULL_RX.is_match(&oid) {
            let page_param: String =
                url::form_urlencoded::byte_serialize(format!("/tv/show/{oid}").as_bytes())
                    .collect();
            let comp_url = format!("https://weibo.com/tv/api/component?page={page_param}");
            let payload =
                serde_json::json!({ "Component_Play_Playinfo": { "oid": oid } }).to_string();
            let body = url::form_urlencoded::Serializer::new(String::new())
                .append_pair("data", &payload)
                .finish();
            let mut post_headers = outgoing.clone();
            post_headers.insert(
                "content-type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            );
            let page = read_page(
                ctx,
                &comp_url,
                &PageRequest {
                    headers: post_headers,
                    method_post_body: Some(body),
                    ..Default::default()
                },
            )
            .await?;
            let json: serde_json::Value = serde_json::from_str(&page.text)?;
            let item = json["data"]["Component_Play_Playinfo"].clone();
            let mut urls: Vec<(u32, String)> = item["urls"]
                .as_object()
                .map(|obj| {
                    obj.iter()
                        .filter_map(|(k, v)| {
                            let s = v.as_str()?;
                            let q = NUM_RX
                                .find(k)
                                .and_then(|m| m.as_str().parse::<u32>().ok())
                                .unwrap_or(0);
                            Some((q, s.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            urls.sort_by(|a, b| b.0.cmp(&a.0));
            let media = urls.into_iter().next().map(|(_, u)| u).unwrap_or_default();
            (item, media)
        } else {
            static STATUS_RX: LazyLock<Regex> =
                LazyLock::new(|| Regex::new(r"/(?:status|detail)/(\w+)").unwrap());
            static USER_STATUS_RX: LazyLock<Regex> =
                LazyLock::new(|| Regex::new(r"^/\d+/(\w+)/?$").unwrap());
            let id = STATUS_RX
                .captures(parsed.path())
                .or_else(|| USER_STATUS_RX.captures(parsed.path()))
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();
            if id.is_empty() {
                return Err(CoreError::msg(
                    "无法解析微博视频 ID，请使用视频分享链接或微博正文链接",
                ));
            }
            let encoded_id: String = url::form_urlencoded::byte_serialize(id.as_bytes()).collect();
            let show_url = format!("https://m.weibo.cn/statuses/show?id={encoded_id}");
            let page = read_page(
                ctx,
                &show_url,
                &PageRequest {
                    headers: outgoing,
                    ..Default::default()
                },
            )
            .await?;
            let json: serde_json::Value = serde_json::from_str(&page.text)?;
            let item = json["data"].clone();
            let media = item["page_info"]["media_info"]["stream_url_hd"]
                .as_str()
                .or_else(|| item["page_info"]["media_info"]["stream_url"].as_str())
                .unwrap_or("")
                .to_string();
            (item, media)
        };

        if media.is_empty() {
            return Err(CoreError::msg(
                "微博未返回可下载视频，请检查 Cookie、内容权限或平台接口变化",
            ));
        }

        let resolved = Url::parse("https://weibo.com/")?.join(&media)?;
        let normalized = normalize_download_url(resolved.as_str())?;
        let title = item["title"]
            .as_str()
            .or_else(|| item["page_info"]["title"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| {
                let raw_text = item["text"].as_str().unwrap_or("");
                TAG_RX.replace_all(raw_text, "").into_owned()
            });

        Ok(VideoInfo {
            url: normalized,
            title,
            platform: Some("weibo".to_string()),
            referer: Some(location),
            author: item["author"]
                .as_str()
                .or_else(|| item["user"]["screen_name"].as_str())
                .map(str::to_string),
            uid: item["author_id"]
                .as_i64()
                .map(|n| n.to_string())
                .or_else(|| item["author_id"].as_str().map(str::to_string))
                .or_else(|| item["user"]["id"].as_i64().map(|n| n.to_string())),
            cover: item["cover_image"]
                .as_str()
                .or_else(|| item["page_info"]["page_pic"]["url"].as_str())
                .map(str::to_string),
            avatar: item["avatar"]
                .as_str()
                .or_else(|| item["user"]["avatar_hd"].as_str())
                .map(str::to_string),
            like: item["like_count"]
                .as_i64()
                .or_else(|| item["attitudes_count"].as_i64()),
            time: None,
            music: None,
        })
    }
    .await;

    match res {
        Ok(info) => Ok(ApiResponse::ok(info)),
        Err(e) if ctx.cancel.is_cancelled() || e.is_aborted() => Err(e),
        Err(e) => Ok(ApiResponse::err(1, format!("微博解析失败：{e}"))),
    }
}
