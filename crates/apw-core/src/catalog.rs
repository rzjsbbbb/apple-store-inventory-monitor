//! 商品与门店目录。
//!
//! 目录有两个来源：随二进制内嵌的离线快照，以及运行时从 Apple 购买页抓来的
//! 最新数据。内嵌快照保证程序在断网或被拦截时仍然可用，在线抓取保证新机发布
//! 当天就能盯，不必等一个新版本。
//!
//! 内嵌用的是 `include_str!`：数据在编译期就进了二进制。上游是运行时按相对
//! 路径去读工作目录下的 `config/*.json`，打包成 .app 之后工作目录未必是仓库
//! 目录，读不到文件就在 `services/store.go:22` 直接 panic 崩掉整个程序。
//!
//! # 这里也有一条「查不到不等于没有」
//!
//! 目录本身不判定库存，但它离那条不变量只有一步：[`Catalog::product_by_part`]
//! 与 [`Catalog::store_by_number`] 找不到时返回 `None`，调用方**必须**把它当成
//! 「这一项暂时说不清」，而不是悄悄删掉监控项或把它显示成无货。上游在这两个
//! 位置（`services/product.go:19`、`services/store.go:22`）用的是
//! `funk.Find(...).(model.Product)` 这样的硬类型断言：找不到时对 nil 接口做
//! 断言，直接 panic 掉整个程序 —— 用户选完商品后正好赶上一次目录刷新、旧型号
//! 不复存在，就足以触发。

use std::collections::{HashMap, HashSet};
use std::sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use serde::Deserialize;

use crate::apple::ApiError;
use crate::model::{Category, Family, Product, Region, Store};

/// 目录相关的失败。
///
/// 刻意分成「内置数据坏了」「地区不存在」「线上抓取失败」几类而不是糊成一个
/// 字符串：界面对它们的处置完全不同 —— 前两者用户重试多少次都没用，最后一种
/// 稍后再试就好。
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// 内置数据里没有这个地区。
    #[error("内置数据里没有地区 {locale} 的{kind}目录")]
    UnknownLocale { locale: String, kind: &'static str },

    /// 内置数据解析失败。只可能是数据文件本身写坏了，与运行环境无关。
    #[error("内置数据 {file} 解析失败：{detail}")]
    Corrupt { file: String, detail: String },

    /// 抓取购买页失败，已按 [`ApiError`] 的口径分类。
    #[error("抓取购买页失败：{0}")]
    Fetch(#[from] ApiError),

    /// 页面拿到了，但里面的商品数据结构与预期不符。
    #[error("购买页结构与预期不符：{detail}")]
    PageSchema { detail: String },

    /// 该地区（在这个品类下）没有配置任何可抓取的购买页。
    #[error("地区 {locale} 的{category}目录没有可抓取的购买页")]
    NoFamilies { locale: String, category: String },

    /// 抓到的门店列表没通过校验，已保留原有数据。
    ///
    /// 与 [`CatalogError::PageSchema`] 分开：那个说的是「页面结构不对」，这个
    /// 说的是「结构对，但内容不可信」。对用户的意思也不同 —— 前者多半要等一个
    /// 新版本，后者稍后重试就好，而且无论哪种，他现在看到的门店列表都还是好的。
    #[error("{locale} 的门店列表未通过校验，继续使用原有数据：{detail}")]
    StoreListRejected { locale: String, detail: String },

    /// 刷新时有机型失败。
    ///
    /// `fetched` 是成功抓到并**已经写进缓存**的型号数：为 0 表示这次刷新毫无
    /// 产出，缓存保持原样。返回错误的同时缓存已被更新，是刻意的 —— iPhone Air
    /// 抓不到不该导致连 iPhone 17 都选不了。
    #[error("地区 {locale} 有 {} 项目录刷新失败（已抓到 {fetched} 个型号）：{}",
            .failures.len(), .failures.join("；"))]
    RefreshFailed {
        locale: String,
        fetched: usize,
        failures: Vec<String>,
    },
}

/// 内嵌的离线商品快照。
///
/// 用 `include_str!` 而不是运行时读文件：打包后那些相对路径根本不存在。
/// 表要手写是因为 `include_str!` 的路径必须是字面量；漏了哪个地区由测试兜住。
const EMBEDDED_PRODUCTS: &[(&str, &str)] = &[
    ("zh_CN", include_str!("../data/products_zh_CN.json")),
    ("zh_HK", include_str!("../data/products_zh_HK.json")),
    ("zh_TW", include_str!("../data/products_zh_TW.json")),
    ("ja_JP", include_str!("../data/products_ja_JP.json")),
    ("en_SG", include_str!("../data/products_en_SG.json")),
    ("en_AU", include_str!("../data/products_en_AU.json")),
    ("en_MY", include_str!("../data/products_en_MY.json")),
];

/// 内嵌的离线门店快照。所有地区都在这一个文件里。
const EMBEDDED_STORES: &str = include_str!("../data/stores.json");

const STORES_FILE: &str = "stores.json";

/// 在线刷新时，门店数相对当前列表的最大允许跌幅。
///
/// 中国大陆是门店最多的地区（四十余家），两成意味着一次少掉十家 —— 现实中
/// 不会发生，所以触发它基本等同于「这份数据有问题」。
const MAX_STORE_SHRINK: f64 = 0.2;

fn products_file(locale: &str) -> String {
    format!("products_{locale}.json")
}

/// 商品与门店目录，可以直接放进 Tauri 的共享状态。
///
/// 全部方法收 `&self`：读路径（界面每次打开下拉框）与写路径（后台刷新）会同时
/// 发生。上游是无锁共享 map（`services/store.go:56` 在后台写 `s.stores`，界面
/// 同时在读），在 Go 里会触发 `fatal error: concurrent map read and map write`，
/// 连 recover 都捕获不到。
///
/// 只有在线刷新结果需要加锁：离线快照在 [`Catalog::new`] 里一次解析完就再也不
/// 变，读它完全不需要同步。
#[derive(Debug)]
pub struct Catalog {
    /// 内嵌快照的解析结果，按地区。
    ///
    /// 解析失败的地区留下错误说明而不是直接丢掉，否则用户只会看到「没有这个
    /// 地区」，永远不知道是数据坏了。存 `String` 而不是 `CatalogError` 是因为
    /// 它要被反复返回，而 `CatalogError` 携带了不可克隆的 [`ApiError`]。
    offline_products: HashMap<&'static str, Result<Vec<Page>, String>>,
    /// 门店的内嵌快照。解析一次就不再变，读它不需要同步。
    offline_stores: Result<HashMap<String, Vec<Store>>, String>,
    /// 在线刷新到的门店，按地区整体覆盖内嵌快照。
    ///
    /// **单位是「地区」，与商品那边的「页」不同。** 商品要按页存是因为抓取按页
    /// 进行、失败也按页发生；门店则是一次请求拿到一整个地区的全量列表，没有
    /// 「这个地区抓到一半」的中间态 —— 要么整份可信地换掉，要么一个字都不动。
    online_stores: RwLock<HashMap<String, Vec<Store>>>,
    /// 在线刷新结果，按「地区 + 购买页」覆盖内嵌快照的同一页。
    ///
    /// **单位必须是「页」，不能是「地区」也不能是「品类」。** 抓取是一页一页
    /// 发请求的，失败也是一页一页失败的：Mac 有八页，其中 macbook-pro 那页
    /// 抓失败时，如果按品类整块覆盖，MacBook Pro 的全部机型就会从目录里凭空
    /// 消失 —— 而返回给用户的只是一句「部分刷新失败」，他不会想到自己盯了
    /// 半天的那台机器已经不在列表里了。按页存之后，失败的那页原样保留旧数据。
    online_products: RwLock<HashMap<(String, PageId), Page>>,
    /// 已完整读取的官网当前机型列表；用于排除离线快照中的历史购买页。
    current_families: RwLock<HashMap<(String, Category), HashSet<String>>>,
}

/// 一个购买页的身份：品类 + slug。
///
/// 和 [`Family`] 说的是同一件事，只是这里的 slug 是 `String`（快照里读出来的）
/// 而不是 `&'static str`。**slug 单独不足以标识一页**：`/shop/buy-mac/xxx` 和
/// `/shop/buy-ipad/xxx` 是两个页面，只拿 slug 当键，两者会在缓存里互相覆盖 ——
/// 表现是刷新完 iPad，Mac 的某一整页商品换成了 iPad 的。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct PageId {
    category: Category,
    slug: String,
}

/// 一页购买页解析出来的商品。
///
/// 内嵌快照和在线刷新都以「页」为单位，两边才对得上：合并时可以逐页决定
/// 「这一页用新的还是用旧的」。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Page {
    id: PageId,
    products: Vec<Product>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self::new()
    }
}

impl Catalog {
    /// 载入内嵌数据。
    ///
    /// 全部地区在这里一次解析完（合计一百多万字节 JSON，几毫秒的事），而不是
    /// 惰性解析：一次性算完之后，离线部分就是一份彻底不可变的数据，读路径无锁、
    /// 也不存在「首次访问时卡在界面线程上」和惰性初始化的重复解析问题。
    ///
    /// 数据坏了也不会失败：那是「这个地区暂时选不了」，不是「整个程序应当立刻
    /// 死掉」，错误留到真正去读那个地区时再报。
    pub fn new() -> Self {
        let offline_products = EMBEDDED_PRODUCTS
            .iter()
            .map(|(locale, text)| (*locale, load_pages(locale, text).map_err(|e| e.to_string())))
            .collect();

        Self {
            offline_products,
            offline_stores: load_stores(EMBEDDED_STORES).map_err(|e| e.to_string()),
            online_stores: RwLock::new(HashMap::new()),
            online_products: RwLock::new(HashMap::new()),
            current_families: RwLock::new(HashMap::new()),
        }
    }

    /// 某地区的全部商品，四个品类合在一起。
    ///
    /// **逐页**决定用哪份数据：刷新过的页用刷新结果，没刷新过的用内嵌快照。
    pub fn products(&self, locale: &str) -> Result<Vec<Product>, CatalogError> {
        // 内嵌快照读不出来就直接报错，哪怕在线已经刷到了几页。
        //
        // 「有几页新数据，先把它们交出去」听着体贴，实际是这个项目最不允许的
        // 那种失败：那几页拼不出完整目录，用户看到的是一个**看起来很正常**的
        // 下拉框，只是里面少了几十个型号，而他无从知道少了。报错至少能让他
        // 知道现在的目录不可信。
        let offline = self.offline_pages(locale)?;
        let online = self.online_pages(locale);

        // 先收在线的、再收只有离线的。顺序在这里是有意义的：同一零件号可能
        // 出现在两页上（Pro 与 Pro Max 共页），下面按零件号去重时**先到的赢**。
        // 反过来排的话，用户刚把某一页刷新完，看到的却还是另一页里那条旧记录，
        // 展示名和容量都是旧的，而界面刚刚才报过「已抓到 N 个型号」。
        let mut sources: Vec<&Page> = Vec::new();
        for page in offline {
            if let Some(fresh) = online.get(&page.id) {
                sources.push(fresh);
            }
        }
        // 内嵌快照里还没有、但已经在线抓到的页也要算进来：新机型刚加进
        // `Region::families`、快照还没重新生成时走的就是这条路 —— 而那正好是
        // 发售当天，最不该看不见新机型的时候。
        //
        // 排序不是为了好看：HashMap 的遍历顺序每次都不一样，不排的话，两页
        // 之间的重复零件号谁赢会随机变化，同一份数据读两次能得到不同结果。
        let mut extra: Vec<&Page> = online
            .values()
            .filter(|page| !offline.iter().any(|p| p.id == page.id))
            .collect();
        extra.sort_by(|a, b| a.id.cmp(&b.id));
        sources.extend(extra);

        for page in offline {
            if !online.contains_key(&page.id) {
                sources.push(page);
            }
        }

        let mut merged: Vec<Product> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let current = read_lock(&self.current_families);
        for page in sources {
            if current
                .get(&(locale.to_string(), page.id.category))
                .is_some_and(|slugs| !slugs.contains(&page.id.slug))
            {
                continue;
            }
            for product in &page.products {
                if seen.insert(product.part_number.clone()) {
                    merged.push(product.clone());
                }
            }
        }

        // 跨页再消歧一次。单页内部的消歧挡不住「两页各出一条、恰好同名」——
        // 那种时候下拉框里会并排摆着两个一字不差的选项，用户只能靠猜。
        crate::apple_catalog::disambiguate_titles(&mut merged);
        sort_products(&mut merged);
        Ok(merged)
    }

    /// 某地区内嵌快照的解析结果。
    fn offline_pages(&self, locale: &str) -> Result<&[Page], CatalogError> {
        match self.offline_products.get(locale) {
            Some(Ok(pages)) => Ok(pages),
            Some(Err(detail)) => Err(CatalogError::Corrupt {
                file: products_file(locale),
                detail: detail.clone(),
            }),
            None => Err(CatalogError::UnknownLocale {
                locale: locale.to_string(),
                kind: "商品",
            }),
        }
    }

    /// 某地区的全部直营店。
    ///
    /// 在线刷新过就用刷到的那份，否则用内嵌快照。**在线失败不影响这里** ——
    /// 刷新失败时 `online_stores` 里根本不会有这个地区的条目，读路径自动落回
    /// 内嵌快照，用户手上的门店列表始终是可用的那一份。
    pub fn stores(&self, locale: &str) -> Result<Vec<Store>, CatalogError> {
        if let Some(stores) = read_lock(&self.online_stores).get(locale) {
            return Ok(stores.clone());
        }

        let by_locale = self
            .offline_stores
            .as_ref()
            .map_err(|detail| CatalogError::Corrupt {
                file: STORES_FILE.to_string(),
                detail: detail.clone(),
            })?;
        by_locale
            .get(locale)
            .cloned()
            .ok_or_else(|| CatalogError::UnknownLocale {
                locale: locale.to_string(),
                kind: "门店",
            })
    }

    /// 按零件号查商品。
    ///
    /// 返回 `Option` 而不是 panic —— 见模块文档里上游那两处硬类型断言。
    /// **找不到不代表这个型号没货**，只代表本地目录里没有它：调用方该提示用户
    /// 目录可能已经过期，而不是把这一项当成无货或者直接删掉。
    pub fn product_by_part(&self, locale: &str, part: &str) -> Option<Product> {
        self.products(locale)
            .ok()?
            .into_iter()
            .find(|p| p.part_number == part)
    }

    /// 按门店编号查门店，语义同 [`Catalog::product_by_part`]。
    pub fn store_by_number(&self, locale: &str, number: &str) -> Option<Store> {
        self.stores(locale)
            .ok()?
            .into_iter()
            .find(|s| s.number == number)
    }

    /// 从 Apple 官网购买页抓最新型号，覆盖该地区的内存副本。返回抓到的型号数。
    ///
    /// 先从所选地区的品类入口发现当前机型，再逐页更新商品。
    /// `category` 为 `None` 时刷新全部品类；界面只传当前品类。
    /// 入口发现失败时回退到已有购买页，同时报告失败，避免冒充完整更新。
    ///
    /// **逐页抓、逐页安装。** 某一页失败时，其余页的新数据照常生效，失败那页
    /// 继续用原来的数据（在线的旧副本或内嵌快照），同时返回
    /// [`CatalogError::RefreshFailed`] —— iPhone Air 抓不到不该导致连 iPhone 17
    /// 都选不了，但也不能装作一切正常，用户得知道自己看到的目录是不全的。
    ///
    /// `http` 必须是宿主长期持有的那一个客户端。上游 `services/listen.go:221`
    /// 每次请求都新建客户端，连接池无法复用，内存能涨到十几 GB。
    pub async fn refresh_products(
        &self,
        region: &'static Region,
        category: Option<Category>,
        http: &reqwest::Client,
    ) -> Result<usize, CatalogError> {
        let families: Vec<&'static Family<'static>> = match category {
            Some(c) => region.families_in(c).collect(),
            None => region.families.iter().collect(),
        };
        if families.is_empty() {
            return Err(CatalogError::NoFamilies {
                locale: region.locale.to_string(),
                category: category.map_or("全部".to_string(), |c| c.title().to_string()),
            });
        }

        let mut fetched: HashSet<String> = HashSet::new();
        let mut failures: Vec<String> = Vec::new();

        let categories: Vec<Category> = Category::ALL
            .iter()
            .copied()
            .filter(|c| families.iter().any(|f| f.category == *c))
            .collect();
        for category in categories {
            let failures_before = failures.len();
            let (slugs, discovered) =
                match crate::apple_catalog::discover_families(http, region, category).await {
                    Ok(slugs) => (slugs, true),
                    Err(err) => {
                        failures.push(format!(
                            "{} 机型发现失败，改用已有购买页：{err}",
                            category.title()
                        ));
                        let mut fallback: Vec<String> = families
                            .iter()
                            .filter(|f| f.category == category)
                            .map(|f| f.slug.to_string())
                            .collect();
                        if let Ok(pages) = self.offline_pages(region.locale) {
                            fallback.extend(
                                pages
                                    .iter()
                                    .filter(|p| p.id.category == category)
                                    .map(|p| p.id.slug.clone()),
                            );
                        }
                        fallback.extend(
                            self.online_pages(region.locale)
                                .keys()
                                .filter(|id| id.category == category)
                                .map(|id| id.slug.clone()),
                        );
                        fallback.sort();
                        fallback.dedup();
                        (fallback, false)
                    }
                };
            for slug in &slugs {
                let family = Family { category, slug };
                match crate::apple_catalog::fetch_products(http, region, &family).await {
                    Ok(items) => {
                        fetched.extend(items.iter().map(|p| p.part_number.clone()));
                        self.install_page(region.locale, &family, items);
                    }
                    Err(err) => failures.push(format!("{slug}：{err}")),
                }
            }
            if discovered && failures.len() == failures_before {
                self.set_current_families(region.locale, category, &slugs);
            }
        }

        if !failures.is_empty() {
            return Err(CatalogError::RefreshFailed {
                locale: region.locale.to_string(),
                fetched: fetched.len(),
                failures,
            });
        }
        Ok(fetched.len())
    }

    /// 从 Apple 官网门店总览页抓最新门店，覆盖该地区的内存副本。返回门店数。
    ///
    /// 与 [`Catalog::refresh_products`] 的差别在于**没有部分成功这回事**：门店
    /// 是一次请求拿到一整个地区的全量列表，要么整份通过校验换掉，要么一个字
    /// 都不动、继续用原来的那份（在线的旧副本或内嵌快照）。
    ///
    /// 刷新失败绝不会让用户的门店列表变空或变短 —— 见 [`Catalog::stores`]。
    ///
    /// # 刷新之后已有监控项怎么办
    ///
    /// 什么都不用做。[`crate::model::Target`] 自带 `store_number` 和
    /// `store_title`，查询和展示都不经过目录；一家店从列表里消失，只意味着它
    /// 不再出现在下拉框里，不会影响已经在盯的那些项。这也是为什么
    /// [`Catalog::store_by_number`] 找不到时返回 `None` 而不是报错。
    pub async fn refresh_stores(
        &self,
        region: &'static Region,
        http: &reqwest::Client,
    ) -> Result<usize, CatalogError> {
        let json = crate::apple_stores::fetch_store_list(http, region).await?;

        // 页面里那段 storeList 与 stores.json 的顶层数组逐字段一致，所以直接走
        // 同一个解析器：去重、城市推导、顺序的那堆地区特例只该存在一份。
        let mut by_locale = load_stores(&json).map_err(|err| match err {
            // 这份数据来自页面而不是 stores.json，照原样报错会指错文件。
            CatalogError::Corrupt { detail, .. } => CatalogError::PageSchema { detail },
            other => other,
        })?;

        let fresh = by_locale.remove(region.locale).ok_or_else(|| {
            // 页面带的是全球门店，本地区不在里面说明拿错了页或者 Apple 调整了
            // 地区划分，都不该拿它去覆盖现有数据。
            CatalogError::StoreListRejected {
                locale: region.locale.to_string(),
                detail: format!("页面里有 {} 个地区，但没有这一个", by_locale.len() + 1),
            }
        })?;

        self.install_stores(region.locale, fresh)
    }

    /// 校验并安装一个地区的门店列表，返回安装后的门店数。
    ///
    /// 校验的用意与写快照脚本时一样：这条路径真正的事故不是报错，而是**悄悄
    /// 装进一份残缺列表** —— 它是合法数据，界面不会有任何异常，用户只是发现
    /// 自己要盯的那家店不见了，而原本好好的那份已经被换掉。所以宁可误拒。
    fn install_stores(&self, locale: &str, fresh: Vec<Store>) -> Result<usize, CatalogError> {
        if fresh.is_empty() {
            return Err(CatalogError::StoreListRejected {
                locale: locale.to_string(),
                detail: "没有解析出任何门店".to_string(),
            });
        }

        // 跌幅校验。Apple 关店是个位数的事，一次少掉两成只可能是抓到了半份页面
        // 或者页面改版 —— 那种时候保留旧数据永远是更安全的一侧。
        //
        // 注意比较的基准是 `stores()` 而不是内嵌快照：连续刷新时，基准应当是
        // 用户此刻实际看到的那份。
        let current = self.stores(locale).map(|s| s.len()).unwrap_or(0);
        let floor = (current as f64 * (1.0 - MAX_STORE_SHRINK)).ceil() as usize;
        if current > 0 && fresh.len() < floor {
            return Err(CatalogError::StoreListRejected {
                locale: locale.to_string(),
                detail: format!(
                    "门店数从 {current} 掉到 {}，跌幅超过 {:.0}%",
                    fresh.len(),
                    MAX_STORE_SHRINK * 100.0
                ),
            });
        }

        let count = fresh.len();
        write_lock(&self.online_stores).insert(locale.to_string(), fresh);
        Ok(count)
    }

    /// 用一页新抓到的商品覆盖该地区同一页的在线副本。
    ///
    /// 空列表直接忽略：把一页清空会让用户已选的监控项在界面上消失，而这通常
    /// 只意味着这一次抓取没拿到东西 —— 那种时候旧数据（哪怕是内嵌兜底）仍然
    /// 比空列表有用。
    fn install_page(&self, locale: &str, family: &Family<'_>, mut products: Vec<Product>) {
        if products.is_empty() {
            return;
        }
        sort_products(&mut products);
        let id = PageId {
            category: family.category,
            slug: family.slug.to_string(),
        };
        write_lock(&self.online_products)
            .insert((locale.to_string(), id.clone()), Page { id, products });
    }

    fn set_current_families(&self, locale: &str, category: Category, slugs: &[String]) {
        if slugs.is_empty() {
            return;
        }
        let active: HashSet<String> = slugs.iter().cloned().collect();
        write_lock(&self.current_families).insert((locale.to_string(), category), active.clone());
        write_lock(&self.online_products).retain(|(l, id), _| {
            l != locale || id.category != category || active.contains(&id.slug)
        });
    }

    /// 某地区已经刷新过的那些页，按页身份索引。
    fn online_pages(&self, locale: &str) -> HashMap<PageId, Page> {
        read_lock(&self.online_products)
            .iter()
            .filter(|((l, _), page)| l == locale && !page.products.is_empty())
            .map(|((_, id), page)| (id.clone(), page.clone()))
            .collect()
    }
}

/// 取读锁，锁中毒时照样把数据交出来。
///
/// 中毒只说明某个线程在持锁期间 panic 了，而这里的临界区只是整体替换一份缓存，
/// 不存在改了一半的中间态。把中毒当成致命错误反而更糟：目录会就此永久不可用，
/// 用户连门店列表都打不开。
fn read_lock<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(PoisonError::into_inner)
}

fn write_lock<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(PoisonError::into_inner)
}

/// 让商品排列稳定且符合直觉：先按品类，再按机型，再按容量从小到大，最后按展示名。
///
/// 原始数据里的顺序是乱的（同一机型下 512GB 可能排在 256GB 前面），直接丢进
/// 下拉框很难找。按品类和机型标识排而不是按数据里的出现顺序，是为了让离线快照
/// 与在线抓取（抓取顺序取决于 `Region::families` 的写法）得到同一份排列。
fn sort_products(products: &mut [Product]) {
    products.sort_by(|a, b| {
        a.category.cmp(&b.category).then_with(|| {
            a.family.cmp(&b.family).then_with(|| {
                crate::apple_catalog::capacity_rank(&a.capacity)
                    .cmp(&crate::apple_catalog::capacity_rank(&b.capacity))
                    .then_with(|| a.title.cmp(&b.title))
            })
        })
    });
}

/// 内嵌快照里的一页。
///
/// `data` 与在线抓取时从购买页里截出来的对象结构完全一致，因此直接复用同一套
/// 解析，不维护两份。外面包一层是因为那个对象本身说不出自己是从哪一页来的 ——
/// Mac 与 Apple Watch 的数据里连机型名都没有，品类和 slug 只能由快照记着。
#[derive(Debug, Deserialize)]
struct SnapshotPage {
    category: Category,
    /// 购买页 slug，如 `macbook-air`。
    family: String,
    data: crate::apple_catalog::ProductSelection,
}

/// 解析某地区的内嵌商品快照。
fn load_pages(locale: &str, text: &str) -> Result<Vec<Page>, CatalogError> {
    let snapshot: Vec<SnapshotPage> =
        serde_json::from_str(text).map_err(|e| CatalogError::Corrupt {
            file: products_file(locale),
            detail: e.to_string(),
        })?;

    let mut pages = Vec::with_capacity(snapshot.len());
    for page in &snapshot {
        let mut products = page.data.to_products(page.category, &page.family);
        if products.is_empty() {
            // 一页都解析不出商品，只可能是快照生成脚本写坏了或者解析口径变了。
            // 放着不管的话，表现是那一页的机型（比如整条 MacBook Pro 产品线）
            // 从离线目录里静默消失，而总数看着还很正常。
            return Err(CatalogError::Corrupt {
                file: products_file(locale),
                detail: format!("{} 这一页没有解析出任何商品", page.family),
            });
        }
        sort_products(&mut products);
        pages.push(Page {
            id: PageId {
                category: page.category,
                slug: page.family.clone(),
            },
            products,
        });
    }

    if pages.is_empty() {
        return Err(CatalogError::Corrupt {
            file: products_file(locale),
            detail: "没有解析出任何商品".to_string(),
        });
    }
    Ok(pages)
}

/// `stores.json` 顶层数组的元素。
///
/// 各地区的结构不统一：`hasStates` 为 true 时门店挂在 `state[].store[]` 下，
/// 否则直接挂在 `store[]` 下。两种都要认。
///
/// 这里的字段写得比购买页那边硬（不额外容忍 null）：这份数据是内嵌的、随二进制
/// 固定，解析结果由测试钉死；购买页的数据来自线上，随时可能变形，所以那边宽容。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawStoreRegion {
    #[serde(default)]
    locale: String,
    #[serde(default)]
    has_states: bool,
    #[serde(default, rename = "state")]
    states: Vec<RawStoreState>,
    #[serde(default, rename = "store")]
    stores: Vec<RawStore>,
}

#[derive(Debug, Deserialize)]
struct RawStoreState {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "store")]
    stores: Vec<RawStore>,
}

#[derive(Debug, Deserialize)]
struct RawStore {
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    address: RawAddress,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAddress {
    #[serde(default)]
    city: String,
    #[serde(default)]
    state_name: String,
}

/// 解析内嵌门店快照，返回按地区分组的门店表。
fn load_stores(text: &str) -> Result<HashMap<String, Vec<Store>>, CatalogError> {
    let regions: Vec<RawStoreRegion> =
        serde_json::from_str(text).map_err(|e| CatalogError::Corrupt {
            file: STORES_FILE.to_string(),
            detail: e.to_string(),
        })?;

    let mut result: HashMap<String, Vec<Store>> = HashMap::with_capacity(regions.len());

    for region in &regions {
        if region.locale.is_empty() {
            continue;
        }

        let mut stores = Vec::new();
        // 按门店编号去重，保留第一次出现的那条，顺序沿用数据源里的顺序 ——
        // 那本来就是按省 / 州分好组的，正合下拉框里找店的直觉。
        let mut seen = HashSet::new();

        for state in &region.states {
            for raw in &state.stores {
                push_store(&mut stores, &mut seen, raw, region.has_states, &state.name);
            }
        }
        for raw in &region.stores {
            push_store(&mut stores, &mut seen, raw, region.has_states, "");
        }

        result.insert(region.locale.clone(), stores);
    }

    if result.is_empty() {
        return Err(CatalogError::Corrupt {
            file: STORES_FILE.to_string(),
            detail: "没有解析出任何地区".to_string(),
        });
    }
    Ok(result)
}

fn push_store(
    out: &mut Vec<Store>,
    seen: &mut HashSet<String>,
    raw: &RawStore,
    has_states: bool,
    state_name: &str,
) {
    let number = raw.id.trim();
    if number.is_empty() {
        return;
    }
    if !seen.insert(number.to_string()) {
        return;
    }

    let name = raw.name.trim();
    // 有 state 层级的地区（中国大陆、日本、澳大利亚等）用 stateName，否则用
    // city。香港、新加坡这类城市站根本没有 stateName 字段。日本站两者都有且
    // 不同（stateName=Tokyo、city=Chiyoda-ku），必须挑前者，否则界面上会冒出
    // 一堆没人认得的区名。
    let mut city = raw.address.city.trim();
    if has_states {
        let from_store = raw.address.state_name.trim();
        if !from_store.is_empty() {
            city = from_store;
        } else if !state_name.trim().is_empty() {
            city = state_name.trim();
        }
    }

    out.push(Store {
        number: number.to_string(),
        name: name.to_string(),
        title: if city.is_empty() {
            name.to_string()
        } else {
            format!("{city}-{name}")
        },
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::REGIONS;

    const IPHONE_17: Family<'static> = Family {
        category: Category::Iphone,
        slug: "iphone-17",
    };
    const IPHONE_18_PRO: Family<'static> = Family {
        category: Category::Iphone,
        slug: "iphone-18-pro",
    };
    const MACBOOK_AIR: Family<'static> = Family {
        category: Category::Mac,
        slug: "macbook-air",
    };

    fn product(part: &str, title: &str) -> Product {
        Product {
            part_number: part.to_string(),
            category: Category::Iphone,
            family: "iphone17".to_string(),
            capacity: "256GB".to_string(),
            color: "黑色".to_string(),
            title: title.to_string(),
        }
    }

    /// 内嵌快照里某一页的全部零件号。
    fn parts_on_page(
        catalog: &Catalog,
        locale: &str,
        category: Category,
        slug: &str,
    ) -> Vec<String> {
        catalog
            .offline_pages(locale)
            .expect("内嵌数据应当可用")
            .iter()
            .find(|p| p.id.category == category && p.id.slug == slug)
            .unwrap_or_else(|| panic!("内嵌快照里没有 {slug} 这一页"))
            .products
            .iter()
            .map(|p| p.part_number.clone())
            .collect()
    }

    /// 某地区目录里的全部零件号。
    fn all_parts(catalog: &Catalog, locale: &str) -> std::collections::BTreeSet<String> {
        catalog
            .products(locale)
            .expect("目录应当可读")
            .into_iter()
            .map(|p| p.part_number)
            .collect()
    }

    #[test]
    fn 内嵌快照覆盖兜底购买页且没有重复页() {
        // 各地区可有额外新品快照，不要求七个站点在同一天上线同一批产品。
        let catalog = Catalog::new();
        for region in REGIONS {
            let pages = catalog
                .offline_pages(region.locale)
                .expect("内嵌数据应当可用");

            let mut actual: Vec<(Category, &str)> = pages
                .iter()
                .map(|p| (p.id.category, p.id.slug.as_str()))
                .collect();
            let mut expected: Vec<(Category, &str)> = region
                .families
                .iter()
                .map(|f| (f.category, f.slug))
                .collect();

            let before = actual.len();
            actual.sort_unstable();
            expected.sort_unstable();
            actual.dedup();
            assert_eq!(actual.len(), before, "{} 的快照里有重复的页", region.locale);
            assert!(
                expected.iter().all(|page| actual.contains(page)),
                "{} 的快照缺少兜底购买页",
                region.locale
            );
        }
    }

    #[test]
    fn 展示名在整个地区内唯一() {
        // 展示名是用户在下拉框里唯一的判断依据。两条不同零件号顶着同一个展示名，
        // 等于让他掷骰子决定监控哪一个 —— 而这个错误直到抢购当天都不会有迹象。
        // 单页内部的消歧挡不住跨页重名，所以这里断言的是合并之后的最终结果。
        let catalog = Catalog::new();
        for region in REGIONS {
            let products = catalog.products(region.locale).expect("内嵌数据应当可用");
            let mut seen: HashMap<&str, &str> = HashMap::new();
            for p in &products {
                if let Some(other) = seen.insert(p.title.as_str(), p.part_number.as_str()) {
                    panic!(
                        "{} 里 {} 和 {} 的展示名都是「{}」",
                        region.locale, other, p.part_number, p.title
                    );
                }
            }
        }
    }

    #[test]
    fn 在线刷新结果会覆盖内嵌的同一页() {
        let catalog = Catalog::new();
        let replaced = parts_on_page(&catalog, "zh_CN", Category::Iphone, "iphone-17");
        assert!(!replaced.is_empty());
        let before = all_parts(&catalog, "zh_CN");
        let ja_before = all_parts(&catalog, "ja_JP");

        catalog.install_page(
            "zh_CN",
            &IPHONE_17,
            vec![product("XXXX/A", "iPhone 18 256GB 黑色")],
        );

        let after = all_parts(&catalog, "zh_CN");
        assert!(after.contains("XXXX/A"));
        assert_eq!(
            catalog.product_by_part("zh_CN", "XXXX/A").map(|p| p.title),
            Some("iPhone 18 256GB 黑色".to_string())
        );
        // 变化必须**恰好**是「那一页被整页换掉」，不多不少。逐条抽查放得太松：
        // 一个错误实现只要保住被抽查的那几条，就能带着别处的破坏蒙混过去。
        let expected: std::collections::BTreeSet<String> = before
            .difference(&replaced.iter().cloned().collect())
            .cloned()
            .chain(std::iter::once("XXXX/A".to_string()))
            .collect();
        assert_eq!(after, expected, "刷新一页之后目录里变动的不止那一页");
        // 只覆盖被刷新的地区，别的地区一个字都不该动。
        assert_eq!(all_parts(&catalog, "ja_JP"), ja_before);
    }

    #[test]
    fn 刷新一页不会动到别的页() {
        // 这是把在线缓存按「页」存的全部理由。Mac 有八页，抓取时 macbook-air
        // 成功、macbook-pro 失败是家常便饭；要是按品类整块覆盖，MacBook Pro 的
        // 全部机型会从目录里凭空消失，而用户只看到一句「部分刷新失败」。
        let catalog = Catalog::new();
        let before = all_parts(&catalog, "zh_CN");
        let replaced = parts_on_page(&catalog, "zh_CN", Category::Mac, "macbook-air");
        assert!(!replaced.is_empty());

        catalog.install_page(
            "zh_CN",
            &MACBOOK_AIR,
            vec![product("XXXX/A", "MacBook Air 15 英寸 银色")],
        );

        let after = all_parts(&catalog, "zh_CN");
        let expected: std::collections::BTreeSet<String> = before
            .difference(&replaced.iter().cloned().collect())
            .cloned()
            .chain(std::iter::once("XXXX/A".to_string()))
            .collect();
        assert_eq!(after, expected, "只刷新了 macbook-air，别的页却也变了");
    }

    #[test]
    fn 同名slug落在不同品类时互不覆盖() {
        // 页的身份是「品类 + slug」。只拿 slug 当键的话，
        // /shop/buy-mac/xxx 和 /shop/buy-ipad/xxx 会在缓存里互相覆盖 ——
        // 表现是刷完 iPad，Mac 里某一整页商品变成了 iPad。
        let catalog = Catalog::new();
        let mac = Family {
            category: Category::Mac,
            slug: "同名",
        };
        let ipad = Family {
            category: Category::Ipad,
            slug: "同名",
        };
        catalog.install_page("zh_CN", &mac, vec![product("MAC/A", "Mac 同名页")]);
        catalog.install_page("zh_CN", &ipad, vec![product("IPAD/A", "iPad 同名页")]);

        let parts = all_parts(&catalog, "zh_CN");
        assert!(parts.contains("MAC/A"), "Mac 那一页被 iPad 顶掉了");
        assert!(parts.contains("IPAD/A"), "iPad 那一页被 Mac 顶掉了");
    }

    #[test]
    fn 同一零件号出现在两页上时以刷新过的那页为准() {
        // 用户刚把某一页刷新完，看到的却还是另一页里那条旧记录 —— 展示名和
        // 容量都是旧的，而界面刚刚才报过「已抓到 N 个型号」。
        let catalog = Catalog::new();
        let shared = parts_on_page(&catalog, "zh_CN", Category::Iphone, "iphone-17")
            .first()
            .expect("iphone-17 页应当有商品")
            .clone();

        let mut fresh = product(&shared, "刷新之后的展示名");
        fresh.category = Category::Iphone;
        catalog.install_page("zh_CN", &IPHONE_18_PRO, vec![fresh]);

        assert_eq!(
            catalog
                .product_by_part("zh_CN", &shared)
                .map(|p| p.title)
                .as_deref(),
            Some("刷新之后的展示名"),
            "同一零件号在两页上时，没刷新的那页压过了刚抓到的数据"
        );
        // 多读几次结果必须一样：谁赢不能取决于 HashMap 的遍历顺序。
        for _ in 0..8 {
            assert_eq!(
                catalog.product_by_part("zh_CN", &shared).map(|p| p.title),
                Some("刷新之后的展示名".to_string())
            );
        }
    }

    #[test]
    fn 空的刷新结果不会把目录清空() {
        let catalog = Catalog::new();
        let before = catalog.products("zh_CN").expect("内嵌数据应当可用");

        catalog.install_page("zh_CN", &IPHONE_17, Vec::new());

        // 一次抓取全军覆没时，旧目录仍然比空目录有用：清空会让用户已选的
        // 监控项在界面上集体消失。
        assert_eq!(catalog.products("zh_CN").expect("仍然可读"), before);
    }

    #[test]
    fn 内嵌快照坏掉时不会拿几页在线数据冒充完整目录() {
        // 「手里有几页新数据，先交出去」听着体贴，实际是这个项目最不允许的
        // 那种失败：用户看到一个**看起来很正常**的下拉框，只是少了几十个型号，
        // 而他无从知道少了。
        let mut catalog = Catalog::new();
        catalog
            .offline_products
            .insert("zh_CN", Err("内嵌数据写坏了".to_string()));

        catalog.install_page(
            "zh_CN",
            &IPHONE_17,
            vec![product("XXXX/A", "iPhone 18 256GB 黑色")],
        );

        match catalog.products("zh_CN") {
            Err(CatalogError::Corrupt { .. }) => {}
            other => panic!("应当报「内嵌数据坏了」，实际是 {other:?}"),
        }
    }

    #[test]
    fn 快照里还没有的新页也能被刷进来() {
        // 新机发布当天走的就是这条路：slug 刚加进 Region::families，快照还没
        // 重新生成。这时候看不见新机型，正好错过唯一要紧的那一天。
        let catalog = Catalog::new();
        let before = all_parts(&catalog, "zh_CN");
        let new_page = Family {
            category: Category::Iphone,
            slug: "iphone-18",
        };

        catalog.install_page(
            "zh_CN",
            &new_page,
            vec![product("XXXX/A", "iPhone 18 256GB 黑色")],
        );

        let mut expected = before.clone();
        expected.insert("XXXX/A".to_string());
        // 只多出新页那一条，内嵌的每一页一个都没少。
        assert_eq!(all_parts(&catalog, "zh_CN"), expected);
    }

    #[test]
    fn 商品按品类机型与容量排序() {
        let catalog = Catalog::new();
        let products = catalog.products("zh_CN").expect("内嵌数据应当可用");

        for pair in products.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            if a.category != b.category {
                assert!(a.category < b.category, "品类排序不对：{a:?} 在 {b:?} 之前");
                continue;
            }
            if a.family != b.family {
                assert!(a.family < b.family, "机型排序不对：{a:?} 在 {b:?} 之前");
                continue;
            }
            let (ra, rb) = (
                crate::apple_catalog::capacity_rank(&a.capacity),
                crate::apple_catalog::capacity_rank(&b.capacity),
            );
            assert!(ra <= rb, "容量排序不对：{} 在 {} 之前", a.title, b.title);
            if ra == rb {
                assert!(a.title <= b.title, "同容量下应当按展示名排");
            }
        }
    }
    #[test]
    fn 完整读取当前机型后旧页面不会从离线快照回流() {
        let catalog = Catalog::new();
        let before = catalog.products("zh_CN").unwrap();
        let old = before
            .iter()
            .find(|p| p.category == Category::Iphone)
            .unwrap()
            .part_number
            .clone();
        let only_current = Family {
            category: Category::Iphone,
            slug: "iphone-current-test",
        };
        catalog.install_page(
            "zh_CN",
            &only_current,
            vec![product("CURRENT/A", "iPhone Current")],
        );
        catalog.set_current_families("zh_CN", Category::Iphone, &[only_current.slug.to_string()]);
        let after = catalog.products("zh_CN").unwrap();
        assert!(after.iter().any(|p| p.part_number == "CURRENT/A"));
        assert!(!after.iter().any(|p| p.part_number == old));
        assert!(after.iter().any(|p| p.category == Category::Mac));
        catalog.set_current_families("zh_CN", Category::Iphone, &[]);
        assert_eq!(after, catalog.products("zh_CN").unwrap());
    }

    // ---- 在线门店刷新 ----
    //
    // 抓取那一半在 apple_stores 里自测；这里钉住的是「什么样的数据允许覆盖
    // 用户手上那份」—— 这条路径真正的风险不是报错，而是悄悄装进一份残缺列表。

    fn store(number: &str, name: &str) -> Store {
        Store {
            number: number.to_string(),
            name: name.to_string(),
            title: format!("测试-{name}"),
        }
    }

    /// 造一份与 `locale` 现有门店数等量的列表，用来绕开跌幅校验。
    fn same_size_as(catalog: &Catalog, locale: &str) -> Vec<Store> {
        let n = catalog.stores(locale).expect("应当有门店").len();
        (0..n)
            .map(|i| store(&format!("RT{i:03}"), &format!("新店{i}")))
            .collect()
    }

    #[test]
    fn 在线门店会覆盖内嵌快照() {
        let catalog = Catalog::new();
        let fresh = same_size_as(&catalog, "zh_CN");
        let count = catalog
            .install_stores("zh_CN", fresh.clone())
            .expect("应当装得进去");

        assert_eq!(count, fresh.len());
        assert_eq!(catalog.stores("zh_CN").unwrap(), fresh);
        // 覆盖是按地区整体进行的，不该波及别的地区。
        assert!(catalog.store_by_number("zh_HK", "R428").is_some());
        // 被换掉的门店从目录里消失，但那是「查不到」，不是错误。
        assert!(catalog.store_by_number("zh_CN", "R683").is_none());
    }

    #[test]
    fn 空列表不会把门店清空() {
        // 与 install_page 同一条规矩：把列表清空会让用户的门店下拉框整个空掉，
        // 而这通常只意味着这一次没抓到东西。
        let catalog = Catalog::new();
        let before = catalog.stores("zh_CN").unwrap();

        let err = catalog
            .install_stores("zh_CN", Vec::new())
            .expect_err("应当被拒绝");
        assert!(matches!(err, CatalogError::StoreListRejected { .. }));
        assert_eq!(catalog.stores("zh_CN").unwrap(), before);
    }

    #[test]
    fn 门店数暴跌会被拒绝且保留原有数据() {
        let catalog = Catalog::new();
        let before = catalog.stores("zh_CN").unwrap();
        assert!(before.len() > 10, "这条测试依赖中国大陆有足够多的门店");

        let err = catalog
            .install_stores("zh_CN", vec![store("RT001", "只剩一家")])
            .expect_err("应当被拒绝");
        let text = err.to_string();
        assert!(text.contains("跌幅"), "{text}");
        // 关键是：被拒绝之后用户手上那份必须原封不动。
        assert_eq!(catalog.stores("zh_CN").unwrap(), before);
    }

    #[test]
    fn 跌幅在允许范围内可以通过() {
        let catalog = Catalog::new();
        let before = catalog.stores("zh_CN").unwrap().len();
        // 关掉一家店是常有的事，不该被校验拦下。
        let fresh: Vec<Store> = (0..before - 1)
            .map(|i| store(&format!("RT{i:03}"), &format!("新店{i}")))
            .collect();

        assert_eq!(
            catalog.install_stores("zh_CN", fresh).expect("应当放行"),
            before - 1
        );
    }

    #[test]
    fn 跌幅基准是当前列表而不是内嵌快照() {
        // 连续刷新时，基准必须跟着走，否则第二次刷新会拿一份早已被替换掉的
        // 快照做比较，放过本该拦下的暴跌。
        let catalog = Catalog::new();
        let embedded = catalog.stores("en_AU").unwrap().len();
        let shrunk: Vec<Store> = (0..embedded - 1)
            .map(|i| store(&format!("RT{i:03}"), &format!("新店{i}")))
            .collect();
        catalog
            .install_stores("en_AU", shrunk)
            .expect("第一次应当放行");

        let now = catalog.stores("en_AU").unwrap().len();
        assert_eq!(now, embedded - 1);
        let floor = (now as f64 * 0.8).ceil() as usize;
        let too_few: Vec<Store> = (0..floor - 1)
            .map(|i| store(&format!("RX{i:03}"), &format!("更少{i}")))
            .collect();
        assert!(catalog.install_stores("en_AU", too_few).is_err());
    }

    #[test]
    fn 没刷新过的地区继续读内嵌快照() {
        let catalog = Catalog::new();
        catalog
            .install_stores("zh_CN", same_size_as(&catalog, "zh_CN"))
            .expect("应当装得进去");

        // 其余地区一个都不该受影响。
        for region in REGIONS.iter().filter(|r| r.locale != "zh_CN") {
            assert_eq!(
                catalog.stores(region.locale).unwrap(),
                Catalog::new().stores(region.locale).unwrap(),
                "{} 的门店不该被动过",
                region.locale
            );
        }
    }

    #[test]
    fn 刷新后重复读取顺序稳定() {
        let catalog = Catalog::new();
        let fresh = same_size_as(&catalog, "ja_JP");
        catalog
            .install_stores("ja_JP", fresh)
            .expect("应当装得进去");
        // 下拉框每次打开都在跳是很明显的退步。
        assert_eq!(
            catalog.stores("ja_JP").unwrap(),
            catalog.stores("ja_JP").unwrap()
        );
    }
}
