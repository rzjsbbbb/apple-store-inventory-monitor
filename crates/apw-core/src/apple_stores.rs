//! 从 Apple 官网门店总览页抓门店目录。
//!
//! 与 [`crate::apple_catalog`] 是对称的两件事：那边刷商品，这边刷门店。在此
//! 之前门店只有内嵌快照一条来源（见 [`crate::catalog`] 里 `offline_stores` 的
//! 注释），Apple 新开一家直营店，用户必须等一个新版本才能选到它。
//!
//! # 这个模块刻意不做归一化
//!
//! 门店总览页 `__NEXT_DATA__` 里 `props.pageProps.storeList` 的结构，与内嵌
//! 快照 `stores.json` 的顶层数组**逐字段一致** —— 后者本来就是从这里取的。
//! 所以这里只负责把那段 JSON 原样截出来，去重、城市推导、顺序一律交回
//! [`crate::catalog`] 里那个已经在用的解析器。
//!
//! 这不是偷懒，是刻意的：门店展示名的构造有一堆地区特例（日本站 `stateName`
//! 与 `city` 不同、香港新加坡根本没有 `stateName`），那套判断只该存在一份。
//! 在线和离线各写一份的话，两边迟早会漂移 —— 而漂移的表现是「同一家店，
//! 刷新前后在界面上叫两个名字」，没有任何报错。
//!
//! # 一个页面就够刷所有地区
//!
//! 这个页面带的是全球 27 个地区的门店，不只请求方那个地区。所以任意一个能
//! 打开的站点都足以刷新任意地区 —— 见 [`crate::model::Region::store_list_url`]。

use std::time::Duration;

use crate::apple::ApiError;
use crate::catalog::CatalogError;
use crate::model::Region;

/// 门店总览页的体积上限。
///
/// 实测各地区站点在 500 KB 上下，2 MB 已经是很宽的余量。与购买页那边的 8 MB
/// 不同：那边要容下整页商品数据，这边只是一张门店表，给太松反而失去了意义。
const MAX_PAGE_BYTES: usize = 2 << 20;

/// 单次取页的超时。挂在请求上而不是客户端上，理由同 [`crate::apple_catalog`]。
const PAGE_TIMEOUT: Duration = Duration::from_secs(20);

/// 单次抓取内部的最大重试次数（不含首次请求）。
const MAX_RETRIES: u32 = 2;

/// 与 `apple.rs`、`apple_catalog.rs` 保持一致的 UA。那两处的常量都是私有的，
/// 引用不到，只能再抄一份。
const PAGE_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// `__NEXT_DATA__` 脚本标签的定位标记。
///
/// 只认 id 属性，不连着匹配 `type="application/json"`：属性顺序和空白都是页面
/// 作者随时会动的东西，多匹配一个字段只是多一个无谓的失败点。
const NEXT_DATA_MARKER: &[u8] = b"id=\"__NEXT_DATA__\"";

const SCRIPT_CLOSE: &[u8] = b"</script>";

/// 抓取门店总览页并截出 `storeList` 那段 JSON。
///
/// 返回的是可以直接交给 [`crate::catalog`] 解析的 JSON 文本，内容是一个数组，
/// 每个元素是一个地区。
///
/// `http` 必须是调用方长期持有的那一个客户端，绝不能在这里现造 ——
/// 理由见 [`crate::apple_catalog`] 的模块文档。
pub async fn fetch_store_list(
    http: &reqwest::Client,
    region: &Region,
) -> Result<String, CatalogError> {
    let url = region.store_list_url();
    let page =
        crate::apple::with_retry(MAX_RETRIES, || fetch_page_once(http, &url, region)).await?;
    extract_store_list(&page)
}

async fn fetch_page_once(
    http: &reqwest::Client,
    url: &str,
    region: &Region,
) -> Result<Vec<u8>, ApiError> {
    let resp = http
        .get(url)
        .timeout(PAGE_TIMEOUT)
        .header(reqwest::header::USER_AGENT, PAGE_USER_AGENT)
        .header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, region.accept_language())
        .header(reqwest::header::REFERER, format!("{}/", region.base_url))
        // 刻意不设置 Accept-Encoding，理由同 apple_catalog：交给 reqwest 的 gzip
        // 特性自动协商并透明解压。
        .send()
        .await
        .map_err(|e| ApiError::Transport(e.to_string()))?;

    let status = resp.status().as_u16();
    let body = crate::apple::read_body_capped(resp, MAX_PAGE_BYTES).await?;

    if let Some(err) = crate::apple::classify_status(status) {
        return Err(err);
    }
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(ApiError::Blocked("HTTP 200 但响应体为空".into()));
    }
    Ok(body)
}

/// 从门店总览页 HTML 中截出 `props.pageProps.storeList`，序列化回 JSON 文本。
///
/// 每一步失败都报 [`CatalogError::PageSchema`] 而不是返回空数组：页面结构变了
/// 需要有人来重新对一遍解析口径，而**空数组会被上层当成一份合法的空目录**，
/// 足以让用户的门店下拉框整个空掉。
pub fn extract_store_list(page: &[u8]) -> Result<String, CatalogError> {
    let marker = find(page, NEXT_DATA_MARKER).ok_or_else(|| CatalogError::PageSchema {
        detail: "门店总览页里没有 __NEXT_DATA__".to_string(),
    })?;

    // 从标记处往后找脚本标签的 `>`，它之后才是 JSON 正文。
    let open = page[marker..]
        .iter()
        .position(|b| *b == b'>')
        .map(|i| marker + i + 1)
        .ok_or_else(|| CatalogError::PageSchema {
            detail: "__NEXT_DATA__ 标签没有闭合".to_string(),
        })?;

    let close = find(&page[open..], SCRIPT_CLOSE)
        .map(|i| open + i)
        .ok_or_else(|| CatalogError::PageSchema {
            detail: "__NEXT_DATA__ 脚本没有结束标签".to_string(),
        })?;

    let raw: serde_json::Value =
        serde_json::from_slice(&page[open..close]).map_err(|e| CatalogError::PageSchema {
            detail: format!("__NEXT_DATA__ 不是合法 JSON：{e}"),
        })?;

    let list = raw
        .get("props")
        .and_then(|v| v.get("pageProps"))
        .and_then(|v| v.get("storeList"))
        .ok_or_else(|| CatalogError::PageSchema {
            detail: "__NEXT_DATA__ 里没有 props.pageProps.storeList".to_string(),
        })?;

    // 空数组和「不是数组」一样，都说明这页不是我们以为的那一页。
    match list.as_array() {
        Some(items) if !items.is_empty() => {}
        Some(_) => {
            return Err(CatalogError::PageSchema {
                detail: "storeList 是空数组".to_string(),
            });
        }
        None => {
            return Err(CatalogError::PageSchema {
                detail: "storeList 不是数组".to_string(),
            });
        }
    }

    serde_json::to_string(list).map_err(|e| CatalogError::PageSchema {
        detail: format!("storeList 无法重新序列化：{e}"),
    })
}

/// 朴素子串查找。页面只有几百 KB，标记也短，不值得引入专门的算法。
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一张最小的门店总览页。
    fn page(store_list: &str) -> Vec<u8> {
        format!(
            r#"<!DOCTYPE html><html><body>
<script id="__NEXT_DATA__" type="application/json">{{"props":{{"pageProps":{{"storeList":{store_list}}}}}}}</script>
</body></html>"#
        )
        .into_bytes()
    }

    const ONE_REGION: &str = r#"[{"locale":"zh_CN","hasStates":true,"state":[{"name":"上海","store":[{"id":"R683","name":"环球港","address":{"city":"上海","stateName":"上海"}}]}]}]"#;

    #[test]
    fn 正常页面能截出门店列表() {
        let json = extract_store_list(&page(ONE_REGION)).expect("应当截得出来");
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("应当是合法 JSON");
        let regions = parsed.as_array().expect("应当是数组");
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0]["locale"], "zh_CN");
    }

    #[test]
    fn 属性顺序变化不影响定位() {
        let raw = format!(
            r#"<script type="application/json" id="__NEXT_DATA__" nonce="x">{{"props":{{"pageProps":{{"storeList":{ONE_REGION}}}}}}}</script>"#
        );
        assert!(extract_store_list(raw.as_bytes()).is_ok());
    }

    #[test]
    fn 没有next_data时报错而不是返回空列表() {
        let err =
            extract_store_list(b"<html><body>nothing here</body></html>").expect_err("应当报错");
        assert!(matches!(err, CatalogError::PageSchema { .. }));
        assert!(err.to_string().contains("__NEXT_DATA__"));
    }

    #[test]
    fn 缺少storelist时报错() {
        let err = extract_store_list(&page_without_store_list()).expect_err("应当报错");
        assert!(err.to_string().contains("storeList"));
    }

    fn page_without_store_list() -> Vec<u8> {
        br#"<script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{}}}</script>"#
            .to_vec()
    }

    #[test]
    fn 空数组报错而不是当成合法空目录() {
        // 这条是这个模块存在的理由之一：空数组一路走下去就是「所有门店都没了」。
        let err = extract_store_list(&page("[]")).expect_err("应当报错");
        assert!(err.to_string().contains("空数组"));
    }

    #[test]
    fn storelist不是数组时报错() {
        let err = extract_store_list(&page(r#"{"zh_CN":[]}"#)).expect_err("应当报错");
        assert!(err.to_string().contains("不是数组"));
    }

    #[test]
    fn json坏掉时报错() {
        let err = extract_store_list(&page("[{")).expect_err("应当报错");
        assert!(err.to_string().contains("不是合法 JSON"));
    }

    #[test]
    fn 脚本没有结束标签时报错() {
        let raw = br#"<script id="__NEXT_DATA__" type="application/json">{"props":{}}"#;
        let err = extract_store_list(raw).expect_err("应当报错");
        assert!(err.to_string().contains("结束标签"));
    }

    #[test]
    fn 每个地区的门店列表地址都能拼出来() {
        use crate::model::REGIONS;
        for region in REGIONS {
            let url = region.store_list_url();
            assert!(
                url.starts_with("https://"),
                "{} 的地址不对：{url}",
                region.locale
            );
            assert!(
                url.ends_with("/retail/storelist/"),
                "{} 的地址不对：{url}",
                region.locale
            );
        }
        // 香港的零售站不在商店站点前缀下面，必须单独绕开。
        let hk = REGIONS
            .iter()
            .find(|r| r.locale == "zh_HK")
            .expect("应当有香港");
        assert_eq!(
            hk.store_list_url(),
            "https://www.apple.com/hk/retail/storelist/"
        );
        assert!(!hk.store_list_url().contains("hk-zh"));
    }
}
