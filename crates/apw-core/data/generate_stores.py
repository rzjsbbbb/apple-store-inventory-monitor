#!/usr/bin/env python3
"""重新生成 stores.json 离线门店快照。

门店与商品不同：[`Catalog`] 没有在线刷新这条路（见 catalog.rs 里
`offline_stores` 的注释），快照里没有的门店，用户在界面上就永远选不到。
所以 Apple 新开或关停一家直营店，只能靠重跑这个脚本 + 发一个新版本。

    python3 crates/apw-core/data/generate_stores.py              # 重新抓取并写入
    python3 crates/apw-core/data/generate_stores.py --self-test  # 不联网，自检解析口径
    python3 crates/apw-core/data/generate_stores.py --dry-run    # 抓取并打印差异，不写文件

数据来自 Apple 官网门店总览页里的 __NEXT_DATA__：

    https://www.apple.com.cn/retail/storelist/
        → props.pageProps.storeList

这一个页面就带着**全球所有地区**的门店列表（27 个 locale），每个元素是一个
`RmdLocale` 对象，结构与 stores.json 的数组元素逐字段一致 —— 本脚本只是把
项目支持的那 7 个地区挑出来，原样写下，不做任何改写。因此 stores.json 始终
是官方载荷的一个忠实子集，日后可以直接和线上数据对 diff。

字段也不做裁剪。`slug`、`telephone`、`address1` 这些 Rust 侧确实没读，但留着
它们才能保证「忠实子集」这个性质；真要瘦身该是另一件事，不该混在数据更新里做。

从哪个站点抓不影响结果：载荷里每个地区的门店名本来就是该地区自己的语言
（日本站是 `Ginza` 不是 `銀座`，台湾站是 `台北 101`），不是按请求站点翻译的。
"""

from pathlib import Path
import argparse
import gzip
import json
import re
import sys
import urllib.error
import urllib.request

UA = (
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36"
)

STORE_LIST_URL = "https://www.apple.com.cn/retail/storelist/"

# 与 crates/apw-core/src/model.rs 的 REGIONS 保持一致（集合一致，顺序未必）。
LOCALES = ["zh_CN", "zh_HK", "zh_TW", "ja_JP", "en_SG", "en_AU", "en_MY"]

# 被测试钉死的门店。它们从快照里消失时，cargo test 会失败 —— 但那是在数据
# 已经被写坏之后。在这里先拦一道，免得一次手滑要靠回滚 git 来收拾。
# 见 crates/apw-core/tests/catalog.rs 的「按编号查门店」。
PINNED = {"zh_CN": "R683", "zh_HK": "R428", "ja_JP": "R718"}

# 门店数相对旧快照的最大允许跌幅。Apple 关店是个位数的事，一次掉两成只可能是
# 抓到了半份页面或者页面改版 —— 那种情况下写文件比不写危险得多。
MAX_SHRINK = 0.2

NEXT_DATA = re.compile(
    rb'<script id="__NEXT_DATA__" type="application/json"[^>]*>(.*?)</script>',
    re.S,
)


def fetch(url: str) -> bytes:
    request = urllib.request.Request(
        url,
        headers={
            "User-Agent": UA,
            "Accept": "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            "Accept-Language": "zh-CN,zh;q=0.9",
            "Accept-Encoding": "gzip",
        },
    )
    with urllib.request.urlopen(request, timeout=40) as response:
        body = response.read()
        if response.headers.get("Content-Encoding") == "gzip":
            body = gzip.decompress(body)
        return body


def extract(page: bytes) -> dict:
    """从门店总览页里取出 locale -> RmdLocale 的表。

    找不到 __NEXT_DATA__、或者里面没有 storeList，都抛异常而不是返回空表：
    这两种情况都意味着页面结构变了，脚本的解析口径需要人来重新对一遍，
    继续跑下去只会把一份空快照写进仓库。
    """
    match = NEXT_DATA.search(page)
    if match is None:
        raise ValueError("页面里没有 __NEXT_DATA__，Apple 可能改版了")

    data = json.loads(match.group(1))
    try:
        store_list = data["props"]["pageProps"]["storeList"]
    except (KeyError, TypeError) as err:
        raise ValueError(f"__NEXT_DATA__ 里没有 props.pageProps.storeList：{err}") from err

    if not isinstance(store_list, list) or not store_list:
        raise ValueError("storeList 不是非空数组")

    return {r["locale"]: r for r in store_list if isinstance(r, dict) and r.get("locale")}


def stores_of(region: dict) -> list:
    """一个地区的全部门店，两种嵌套结构都认。

    与 catalog.rs 的 `load_stores` 同口径：`hasStates` 为 true 时门店挂在
    `state[].store[]` 下，否则直接挂在 `store[]` 下。这里不去读 `hasStates`
    而是两处都收 —— 校验要的是「一共有多少店」，宁可多认不能漏认。
    """
    out = []
    for state in region.get("state") or []:
        out.extend(state.get("store") or [])
    out.extend(region.get("store") or [])
    return out


def check(fresh: dict, previous: list | None) -> list[str]:
    """返回所有拒绝写入的理由；空列表表示这份数据可以落盘。

    这个脚本最容易出的事故不是崩掉，而是**悄悄写进一份残缺快照**：它是合法
    JSON、门店也非空，Rust 加载和全部测试都会通过，用户只是发现自己那家店
    不在下拉框里了 —— 而工作区里那份完好的旧快照已经被盖掉。
    所以这里宁可误报，也不放过。
    """
    problems = []
    old_counts = {}
    if previous is not None:
        old_counts = {r["locale"]: len(stores_of(r)) for r in previous}

    for locale in LOCALES:
        region = fresh.get(locale)
        if region is None:
            problems.append(f"{locale}：官网门店列表里没有这个地区")
            continue

        stores = stores_of(region)
        if not stores:
            problems.append(f"{locale}：一家门店都没解析出来")
            continue

        ids = set()
        for store in stores:
            number = (store.get("id") or "").strip()
            if not number:
                # Rust 侧 push_store 会静默跳过空编号，于是这家店只是「不见了」。
                problems.append(f"{locale}：有门店的 id 为空（name={store.get('name')!r}）")
                continue
            if number in ids:
                problems.append(f"{locale}：门店编号 {number} 在同一地区里重复")
            ids.add(number)

        pinned = PINNED.get(locale)
        if pinned and pinned not in ids:
            problems.append(f"{locale}：测试钉死的门店 {pinned} 不在新数据里")

        before = old_counts.get(locale)
        if before and len(stores) < before * (1 - MAX_SHRINK):
            problems.append(
                f"{locale}：门店数从 {before} 掉到 {len(stores)}，跌幅超过 {MAX_SHRINK:.0%}"
            )

    return problems


def diff(previous: list, fresh: dict) -> None:
    """把变化打印出来。更新门店数据本来就该有人看一眼再提交。"""
    for region in previous:
        locale = region["locale"]
        if locale not in fresh:
            continue
        old = {s["id"]: s.get("name", "") for s in stores_of(region) if s.get("id")}
        new = {s["id"]: s.get("name", "") for s in stores_of(fresh[locale]) if s.get("id")}

        added = sorted(set(new) - set(old))
        removed = sorted(set(old) - set(new))
        renamed = sorted(k for k in set(old) & set(new) if old[k] != new[k])

        if not (added or removed or renamed):
            print(f"  {locale}: {len(new)} 家，无变化")
            continue

        print(f"  {locale}: {len(old)} -> {len(new)} 家")
        for k in added:
            print(f"      + {k} {new[k]}")
        for k in removed:
            print(f"      - {k} {old[k]}")
        for k in renamed:
            print(f"      ~ {k} {old[k]} -> {new[k]}")


# ---- 自检 ----
#
# 同 generate.py：这个脚本的产出没法靠 Rust 测试兜住，所以最容易出事的那几处
# 判断在这里自己测，不联网。
#
#     python3 crates/apw-core/data/generate_stores.py --self-test


def self_test() -> int:
    failures = []

    def check_that(name: str, ok: bool) -> None:
        if not ok:
            failures.append(name)

    # 两种嵌套结构都要认全。
    with_states = {
        "locale": "zh_CN",
        "hasStates": True,
        "state": [{"name": "上海", "store": [{"id": "R683", "name": "环球港"}]}],
    }
    flat = {
        "locale": "zh_HK",
        "hasStates": False,
        "store": [{"id": "R428", "name": "ifc mall"}],
    }
    check_that("hasStates 地区应当收到 state[].store[]", len(stores_of(with_states)) == 1)
    check_that("无 state 地区应当收到 store[]", len(stores_of(flat)) == 1)
    check_that("缺字段不应当崩", stores_of({"locale": "x"}) == [])

    # 校验必须拦住各种残缺。
    good = {
        "zh_CN": with_states,
        "zh_HK": flat,
        "zh_TW": {"locale": "zh_TW", "store": [{"id": "R713", "name": "台北 101"}]},
        "ja_JP": {"locale": "ja_JP", "store": [{"id": "R718", "name": "Kyoto"}]},
        "en_SG": {"locale": "en_SG", "store": [{"id": "R633", "name": "Orchard Road"}]},
        "en_AU": {"locale": "en_AU", "store": [{"id": "R237", "name": "Sydney"}]},
        "en_MY": {"locale": "en_MY", "store": [{"id": "R790", "name": "TRX"}]},
    }
    check_that("完好数据应当通过", check(good, None) == [])

    missing = {k: v for k, v in good.items() if k != "en_AU"}
    check_that("缺地区应当被拦下", any("en_AU" in p for p in check(missing, None)))

    empty_id = json.loads(json.dumps(good))
    empty_id["en_MY"]["store"][0]["id"] = ""
    check_that("空编号应当被拦下", any("id 为空" in p for p in check(empty_id, None)))

    dropped_pin = json.loads(json.dumps(good))
    dropped_pin["zh_CN"]["state"][0]["store"][0]["id"] = "R999"
    check_that("钉死门店丢失应当被拦下", any("R683" in p for p in check(dropped_pin, None)))

    dup = json.loads(json.dumps(good))
    dup["zh_HK"]["store"].append({"id": "R428", "name": "重复"})
    check_that("重复编号应当被拦下", any("重复" in p for p in check(dup, None)))

    # 跌幅校验：旧快照 10 家、新数据 1 家，必须拒绝。
    shrunk_prev = [{"locale": "en_AU", "store": [{"id": f"R{i:03d}"} for i in range(10)]}]
    check_that(
        "门店数暴跌应当被拦下",
        any("跌幅" in p for p in check(good, shrunk_prev)),
    )

    # 顺序必须跟着旧文件走，否则 diff 会被整段位移淹掉。
    shuffled = [{"locale": lc} for lc in ["zh_CN", "zh_HK", "zh_TW", "en_SG", "ja_JP", "en_AU", "en_MY"]]
    check_that(
        "应当沿用旧文件的地区顺序",
        output_order(shuffled) == [r["locale"] for r in shuffled],
    )
    check_that("没有旧文件时退回 LOCALES 顺序", output_order(None) == LOCALES)
    check_that(
        "旧文件缺地区时补在末尾",
        output_order([{"locale": "en_AU"}]) == ["en_AU"] + [lc for lc in LOCALES if lc != "en_AU"],
    )

    # 解析器对坏页面必须抛异常，不能返回空表。
    for bad, why in [(b"<html></html>", "没有 __NEXT_DATA__"), (
        b'<script id="__NEXT_DATA__" type="application/json">{"props":{}}</script>',
        "没有 storeList",
    )]:
        try:
            extract(bad)
        except ValueError:
            pass
        else:
            failures.append(f"坏页面（{why}）应当抛异常")

    for name in failures:
        print(f"自检失败：{name}", file=sys.stderr)
    if failures:
        return 1
    print(f"自检通过（{len(LOCALES)} 个地区）")
    return 0


def output_order(previous: list | None) -> list[str]:
    """决定写出时的地区顺序。

    **沿用旧文件里的顺序**，而不是 LOCALES 的顺序。数组顺序不参与任何逻辑
    （Rust 侧按 locale 建表），但它决定 diff 好不好读：仓库里那份的顺序恰好
    不是 REGIONS 的顺序，照 LOCALES 重排会把一次「三家门店增减」变成三百行
    的 diff，真正的改动淹没在整段整段的位移里，评审时根本看不出来。

    旧文件不存在（第一次生成）时才退回 LOCALES 顺序。
    """
    if not previous:
        return list(LOCALES)
    seen = [r["locale"] for r in previous if r.get("locale") in LOCALES]
    return seen + [lc for lc in LOCALES if lc not in seen]


def main() -> int:
    parser = argparse.ArgumentParser(description="重新生成 stores.json")
    parser.add_argument("--self-test", action="store_true", help="不联网，自检解析口径")
    parser.add_argument("--dry-run", action="store_true", help="抓取并打印差异，但不写文件")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    target = Path(__file__).resolve().parent / "stores.json"
    previous = None
    if target.exists():
        previous = json.loads(target.read_text(encoding="utf-8"))

    print(f"抓取 {STORE_LIST_URL}")
    try:
        fresh = extract(fetch(STORE_LIST_URL))
    except (urllib.error.URLError, OSError, ValueError, json.JSONDecodeError) as err:
        print(f"抓取失败：{err}", file=sys.stderr)
        return 1
    print(f"  官网共 {len(fresh)} 个地区，项目支持 {len(LOCALES)} 个")

    problems = check(fresh, previous)
    if problems:
        # 一处不对就一个字都不写 —— 见 check() 的文档。
        print("\n数据校验未通过，保持 stores.json 原样不覆盖：", file=sys.stderr)
        for p in problems:
            print(f"  !! {p}", file=sys.stderr)
        return 1

    if previous is not None:
        print("\n变化：")
        diff(previous, fresh)

    if args.dry_run:
        print("\n--dry-run：未写入文件")
        return 0

    regions = [fresh[locale] for locale in output_order(previous)]
    target.write_text(
        json.dumps(regions, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    total = sum(len(stores_of(r)) for r in regions)
    print(f"\n{target.name}: {len(regions)} 个地区，{total} 家门店，{target.stat().st_size} 字节")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
