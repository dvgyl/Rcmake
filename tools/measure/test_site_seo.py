# SPDX-License-Identifier: MIT OR Apache-2.0
"""Validate site SEO metadata, crawlers, sitemap, LLMs.txt, and JSON-LD."""

from __future__ import annotations

from html.parser import HTMLParser
import json
from pathlib import Path
import struct
import unittest
from urllib.parse import urlparse
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[2]
SITE_DIR = ROOT / "site"
SITE_ORIGIN = "https://rcmake.github.io/rcmake/"
REPO_URL = "https://github.com/rcmake/rcmake"

SITEMAP_NS = {"sm": "http://www.sitemaps.org/schemas/sitemap/0.9"}
EM_DASH = "\u2014"
EN_DASH = "\u2013"
CURLY_QUOTES = ("\u2018", "\u2019", "\u201c", "\u201d")

REQUIRED_CRAWLERS = (
    "*",
    "Googlebot",
    "Bingbot",
    "Applebot",
    "OAI-SearchBot",
    "GPTBot",
    "ChatGPT-User",
    "ClaudeBot",
    "Claude-SearchBot",
    "Claude-User",
    "PerplexityBot",
    "Perplexity-User",
    "Google-Extended",
)

EXPECTED_SITEMAP = {
    f"{SITE_ORIGIN}": "1.0",
    f"{SITE_ORIGIN}playground.html": "0.9",
    f"{SITE_ORIGIN}dashboard.html": "0.9",
    f"{SITE_ORIGIN}llms.txt": "0.8",
    f"{SITE_ORIGIN}llms-full.txt": "0.7",
    f"{SITE_ORIGIN}data/latest.json": "0.6",
}

EXPECTED_PAGES = {
    "index.html": {
        "title": "rcmake, a drop-in CMake 4.5.1 replacement in safe Rust",
        "canonical": SITE_ORIGIN,
    },
    "playground.html": {
        "title": "rcmake WebAssembly playground for CMake 4.5.1 and Ninja",
        "canonical": f"{SITE_ORIGIN}playground.html",
    },
    "dashboard.html": {
        "title": "rcmake compatibility report for CMake 4.5.1 parity and benchmarks",
        "canonical": f"{SITE_ORIGIN}dashboard.html",
    },
}

FAKE_VERIFICATION_META = {
    "google-site-verification",
    "msvalidate.01",
    "yandex-verification",
    "baidu-site-verification",
    "facebook-domain-verification",
}


def normalize_ws(text: str) -> str:
    return " ".join(text.split())


def resolve_site_url(url: str) -> Path | None:
    if not url.startswith(SITE_ORIGIN):
        return None
    parsed = urlparse(url)
    base_path = urlparse(SITE_ORIGIN).path
    rel = parsed.path[len(base_path):]
    if not rel or rel.endswith("/"):
        rel = f"{rel}index.html"
    return SITE_DIR / rel


class PageSeoParser(HTMLParser):
    """Extract SEO tags, JSON-LD blocks, noscript text, and visible FAQ items."""

    def __init__(self) -> None:
        super().__init__(convert_charrefs=True)
        self.title_parts: list[str] = []
        self.meta_by_name: dict[str, str] = {}
        self.meta_by_prop: dict[str, str] = {}
        self.links: list[dict[str, str]] = []
        self.jsonld_blocks: list[str] = []
        self.noscript_blocks: list[str] = []
        self.faq_items: list[tuple[str, str]] = []

        self._in_title = False
        self._in_jsonld = False
        self._jsonld_buf: list[str] = []
        self._noscript_depth = 0
        self._noscript_buf: list[str] = []

        self._faq_section_depth = 0
        self._in_faq_details = False
        self._in_faq_summary = False
        self._faq_answer_depth = 0
        self._cur_summary: list[str] = []
        self._cur_answer: list[str] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        attr_map = {k.lower(): (v or "") for k, v in attrs}
        t = tag.lower()

        if t == "title":
            self._in_title = True
        elif t == "meta":
            name = attr_map.get("name", "").lower()
            prop = attr_map.get("property", "").lower()
            content = attr_map.get("content", "")
            if name:
                self.meta_by_name[name] = content
            if prop:
                self.meta_by_prop[prop] = content
        elif t == "link":
            self.links.append(attr_map)
        elif t == "script" and attr_map.get("type", "").lower() == "application/ld+json":
            self._in_jsonld = True
            self._jsonld_buf = []
        elif t == "noscript":
            self._noscript_depth += 1
            if self._noscript_depth == 1:
                self._noscript_buf = []

        if t == "section" and attr_map.get("id") == "faq":
            self._faq_section_depth = 1
        elif self._faq_section_depth > 0:
            if t == "section":
                self._faq_section_depth += 1
            elif t == "details" and "faq-item" in attr_map.get("class", "").split():
                self._in_faq_details = True
                self._cur_summary = []
                self._cur_answer = []
            elif self._in_faq_details and t == "summary":
                self._in_faq_summary = True
            elif self._in_faq_details and t == "div" and "faq-answer" in attr_map.get("class", "").split():
                self._faq_answer_depth = 1
            elif self._faq_answer_depth > 0 and t == "div":
                self._faq_answer_depth += 1

    def handle_endtag(self, tag: str) -> None:
        t = tag.lower()
        if t == "title":
            self._in_title = False
        elif t == "script" and self._in_jsonld:
            self.jsonld_blocks.append("".join(self._jsonld_buf))
            self._in_jsonld = False
            self._jsonld_buf = []
        elif t == "noscript" and self._noscript_depth > 0:
            self._noscript_depth -= 1
            if self._noscript_depth == 0:
                self.noscript_blocks.append(normalize_ws("".join(self._noscript_buf)))
                self._noscript_buf = []

        if self._faq_section_depth > 0:
            if self._in_faq_summary and t == "summary":
                self._in_faq_summary = False
            elif self._faq_answer_depth > 0 and t == "div":
                self._faq_answer_depth -= 1
            elif self._in_faq_details and t == "details":
                self.faq_items.append(
                    (normalize_ws("".join(self._cur_summary)), normalize_ws("".join(self._cur_answer)))
                )
                self._in_faq_details = False
            elif t == "section":
                self._faq_section_depth -= 1

    def handle_data(self, data: str) -> None:
        if self._in_title:
            self.title_parts.append(data)
        if self._in_jsonld:
            self._jsonld_buf.append(data)
        if self._noscript_depth > 0:
            self._noscript_buf.append(data)
        if self._in_faq_summary:
            self._cur_summary.append(data)
        if self._faq_answer_depth > 0:
            self._cur_answer.append(data)

    @property
    def title(self) -> str:
        return normalize_ws("".join(self.title_parts))


def parse_page(filename: str) -> tuple[str, PageSeoParser]:
    raw = (SITE_DIR / filename).read_text(encoding="utf-8")
    parser = PageSeoParser()
    parser.feed(raw)
    parser.close()
    return raw, parser


def collect_urls(obj: object) -> list[str]:
    found: list[str] = []
    if isinstance(obj, str):
        if obj.startswith("http://") or obj.startswith("https://"):
            found.append(obj)
    elif isinstance(obj, list):
        for item in obj:
            found.extend(collect_urls(item))
    elif isinstance(obj, dict):
        for key, value in obj.items():
            if key == "@context":
                continue
            found.extend(collect_urls(value))
    return found


class SiteSeoTests(unittest.TestCase):
    def test_robots_txt(self) -> None:
        robots_path = SITE_DIR / "robots.txt"
        self.assertTrue(robots_path.is_file(), "Missing site/robots.txt")
        text = robots_path.read_text(encoding="utf-8")
        self.assertNotIn(EM_DASH, text)
        self.assertIn(f"Sitemap: {SITE_ORIGIN}sitemap.xml", text)
        self.assertIn(f"{SITE_ORIGIN}llms.txt", text)

        rules: dict[str, list[tuple[str, str]]] = {}
        current_agents: list[str] = []
        for raw_line in text.splitlines():
            line = raw_line.split("#", 1)[0].strip()
            if not line:
                current_agents = []
                continue
            key, sep, val = line.partition(":")
            if not sep:
                continue
            k = key.strip().lower()
            v = val.strip()
            if k == "user-agent":
                current_agents.append(v)
                rules.setdefault(v, [])
            elif k in ("allow", "disallow"):
                for agent in current_agents:
                    rules.setdefault(agent, []).append((k, v))

        for crawler in REQUIRED_CRAWLERS:
            self.assertIn(crawler, rules, f"robots.txt missing User-agent: {crawler}")
            self.assertIn(("allow", "/"), rules[crawler], f"robots.txt must Allow: / for {crawler}")
            self.assertNotIn(("disallow", "/"), rules[crawler], f"robots.txt must not Disallow: / for {crawler}")

    def test_sitemap_xml(self) -> None:
        sitemap_path = SITE_DIR / "sitemap.xml"
        self.assertTrue(sitemap_path.is_file(), "Missing site/sitemap.xml")
        tree = ET.parse(sitemap_path)
        root = tree.getroot()
        self.assertEqual(root.tag, f"{{{SITEMAP_NS['sm']}}}urlset")

        found: dict[str, str] = {}
        for url_el in root.findall("sm:url", SITEMAP_NS):
            loc = url_el.findtext("sm:loc", default="", namespaces=SITEMAP_NS).strip()
            priority = url_el.findtext("sm:priority", default="", namespaces=SITEMAP_NS).strip()
            self.assertTrue(loc, "Empty <loc> in sitemap.xml")
            self.assertNotIn(loc, found, f"Duplicate URL in sitemap.xml: {loc}")
            found[loc] = priority

            local_file = resolve_site_url(loc)
            self.assertIsNotNone(local_file, f"Sitemap URL outside canonical origin: {loc}")
            assert local_file is not None
            self.assertTrue(local_file.is_file(), f"Sitemap URL target missing on disk: {local_file}")

        self.assertEqual(found, EXPECTED_SITEMAP)

    def test_llms_txt_and_full_reference(self) -> None:
        llms_path = SITE_DIR / "llms.txt"
        full_path = SITE_DIR / "llms-full.txt"
        self.assertTrue(llms_path.is_file(), "Missing site/llms.txt")
        self.assertTrue(full_path.is_file(), "Missing site/llms-full.txt")

        llms = llms_path.read_text(encoding="utf-8")
        full = full_path.read_text(encoding="utf-8")

        self.assertTrue(llms.startswith("# rcmake\n\n> "), "llms.txt must start with H1 and blockquote summary")
        self.assertTrue(full.startswith("# rcmake "), "llms-full.txt must start with an H1 heading")
        self.assertGreater(len(full), len(llms) * 2, "llms-full.txt should be more detailed than llms.txt")

        forbidden_markers = (
            "<!-- METRIC:",
            "pending frozen release run",
            "TBD",
            "TODO",
            "FIXME",
            "XX%",
            "99.9%",
            "10x faster",
        )
        for name, content in (("llms.txt", llms), ("llms-full.txt", full)):
            self.assertNotIn(EM_DASH, content, f"Em dash found in {name}")
            self.assertNotIn(EN_DASH, content, f"En dash found in {name}")
            for q in CURLY_QUOTES:
                self.assertNotIn(q, content, f"Curly quote found in {name}")
            for marker in forbidden_markers:
                self.assertNotIn(marker, content, f"Placeholder or unverified claim {marker!r} in {name}")
            self.assertNotRegex(content, r"/(?:Users|home)/", f"Developer home path found in {name}")
            for required_fact in (
                "CMake 4.5.1",
                "cmake",
                "ctest",
                "cpack",
                "Ninja",
                "Xcode",
                "Visual Studio 17 2022",
                "explain",
                "--profile",
                "--hermetic",
                "--fetch-lock",
                "rcmake-lsp",
                "WebAssembly",
                "MIT OR Apache-2.0",
                "BSD-3-Clause",
                "https://rcmake.github.io/rcmake/",
                REPO_URL,
            ):
                self.assertIn(required_fact, content, f"Missing required fact {required_fact!r} in {name}")

        self.assertIn(f"{SITE_ORIGIN}llms-full.txt", llms)
        self.assertIn(f"{SITE_ORIGIN}playground.html", llms)
        self.assertIn(f"{SITE_ORIGIN}dashboard.html", llms)

    def test_manifest_webmanifest(self) -> None:
        manifest_path = SITE_DIR / "manifest.webmanifest"
        self.assertTrue(manifest_path.is_file(), "Missing site/manifest.webmanifest")
        data = json.loads(manifest_path.read_text(encoding="utf-8"))
        self.assertEqual(data.get("name"), "rcmake")
        self.assertEqual(data.get("short_name"), "rcmake")
        icons = data.get("icons", [])
        self.assertTrue(icons, "manifest.webmanifest must list at least one icon")
        for icon in icons:
            src = icon.get("src", "")
            self.assertTrue((SITE_DIR / src).is_file(), f"Manifest icon missing: {src}")

    def test_html_meta_og_twitter_links_and_noscript(self) -> None:
        for filename, expected in EXPECTED_PAGES.items():
            raw, page = parse_page(filename)
            self.assertNotIn(EM_DASH, raw, f"Em dash found in {filename}")
            self.assertEqual(page.title, expected["title"], f"Unexpected <title> in {filename}")

            desc = page.meta_by_name.get("description", "")
            self.assertTrue(80 <= len(desc) <= 170, f"Description length {len(desc)} out of range in {filename}")

            robots = page.meta_by_name.get("robots", "")
            for directive in ("index", "follow", "max-image-preview:large", "max-snippet:-1"):
                self.assertIn(directive, robots, f"Missing robots directive {directive} in {filename}")

            for token_name in FAKE_VERIFICATION_META:
                self.assertNotIn(token_name, page.meta_by_name, f"Unexpected verification meta {token_name} in {filename}")

            canonicals = [link.get("href") for link in page.links if link.get("rel") == "canonical"]
            self.assertEqual(canonicals, [expected["canonical"]], f"Canonical link mismatch in {filename}")

            llms_alts = [
                link for link in page.links
                if link.get("rel") == "alternate"
                and link.get("type") == "text/plain"
                and link.get("href") == f"{SITE_ORIGIN}llms.txt"
            ]
            self.assertEqual(len(llms_alts), 1, f"Missing LLMs.txt alternate link in {filename}")

            manifests = [link.get("href") for link in page.links if link.get("rel") == "manifest"]
            self.assertEqual(manifests, ["./manifest.webmanifest"], f"Missing manifest link in {filename}")

            for prop in (
                "og:site_name",
                "og:type",
                "og:title",
                "og:description",
                "og:url",
                "og:image",
                "og:image:width",
                "og:image:height",
                "og:image:alt",
            ):
                self.assertTrue(page.meta_by_prop.get(prop), f"Missing {prop} in {filename}")

            self.assertEqual(page.meta_by_prop["og:site_name"], "rcmake")
            self.assertEqual(page.meta_by_prop["og:url"], expected["canonical"])
            self.assertEqual(page.meta_by_prop["og:title"], expected["title"])
            self.assertEqual(page.meta_by_prop["og:description"], desc)
            self.assertEqual(page.meta_by_prop["og:image:width"], "1200")
            self.assertEqual(page.meta_by_prop["og:image:height"], "630")
            og_img = resolve_site_url(page.meta_by_prop["og:image"])
            self.assertIsNotNone(og_img)
            assert og_img is not None
            self.assertTrue(og_img.is_file(), f"og:image missing on disk: {og_img}")
            png_header = og_img.read_bytes()[:24]
            self.assertEqual(png_header[:8], b"\x89PNG\r\n\x1a\n", f"og:image is not a valid PNG: {og_img}")
            width, height = struct.unpack(">II", png_header[16:24])
            self.assertEqual((str(width), str(height)), (page.meta_by_prop["og:image:width"], page.meta_by_prop["og:image:height"]))

            for name in (
                "twitter:card",
                "twitter:title",
                "twitter:description",
                "twitter:image",
                "twitter:image:alt",
            ):
                self.assertTrue(page.meta_by_name.get(name), f"Missing {name} in {filename}")
            self.assertEqual(page.meta_by_name["twitter:card"], "summary_large_image")
            self.assertEqual(page.meta_by_name["twitter:title"], expected["title"])
            self.assertEqual(page.meta_by_name["twitter:description"], desc)
            self.assertEqual(page.meta_by_name["twitter:image"], page.meta_by_prop["og:image"])

            self.assertTrue(page.noscript_blocks, f"Missing <noscript> block in {filename}")
            combined_noscript = " ".join(page.noscript_blocks)
            self.assertIn("llms.txt", combined_noscript, f"<noscript> should reference llms.txt in {filename}")
            if filename == "dashboard.html":
                self.assertIn("data/latest.json", combined_noscript)
            if filename == "index.html":
                self.assertIn("GitHub", combined_noscript)

    def test_index_jsonld_and_faq_exact_parity(self) -> None:
        _, page = parse_page("index.html")
        self.assertEqual(len(page.jsonld_blocks), 1, "index.html must contain one JSON-LD script block")
        payload = json.loads(page.jsonld_blocks[0])
        self.assertEqual(payload.get("@context"), "https://schema.org")
        graph = payload.get("@graph", [])
        by_type = {node.get("@type"): node for node in graph if isinstance(node, dict)}

        self.assertIn("WebSite", by_type)
        self.assertEqual(by_type["WebSite"].get("name"), "rcmake")
        self.assertEqual(by_type["WebSite"].get("url"), SITE_ORIGIN)

        self.assertIn("SoftwareApplication", by_type)
        app = by_type["SoftwareApplication"]
        self.assertEqual(app.get("name"), "rcmake")
        self.assertEqual(app.get("applicationCategory"), "DeveloperApplication")
        self.assertEqual(app.get("operatingSystem"), "macOS, Linux, Windows, WebAssembly")
        self.assertEqual(app.get("softwareVersion"), "0.2.0")
        self.assertEqual(
            app.get("license"),
            [
                "https://spdx.org/licenses/MIT.html",
                "https://spdx.org/licenses/Apache-2.0.html",
            ],
        )
        self.assertEqual(app.get("programmingLanguage"), "Rust")
        self.assertEqual(app.get("codeRepository"), REPO_URL)
        self.assertTrue(app.get("downloadUrl"))
        self.assertIsInstance(app.get("featureList"), list)
        self.assertGreaterEqual(len(app["featureList"]), 6)

        self.assertIn("FAQPage", by_type)
        faq_entities = by_type["FAQPage"].get("mainEntity", [])
        self.assertEqual(len(faq_entities), 8, "FAQPage must have exactly 8 questions")
        self.assertEqual(len(page.faq_items), 8, "Visible #faq section must have exactly 8 <details class='faq-item'>")

        for idx, (entity, (vis_q, vis_a)) in enumerate(zip(faq_entities, page.faq_items), start=1):
            self.assertEqual(entity.get("@type"), "Question", f"FAQ #{idx} missing @type Question")
            ans = entity.get("acceptedAnswer", {})
            self.assertEqual(ans.get("@type"), "Answer", f"FAQ #{idx} missing @type Answer")
            json_q = normalize_ws(entity.get("name", ""))
            json_a = normalize_ws(ans.get("text", ""))
            self.assertEqual(json_q, vis_q, f"FAQ #{idx} question text mismatch between JSON-LD and visible <summary>")
            self.assertEqual(json_a, vis_a, f"FAQ #{idx} answer text mismatch between JSON-LD and visible #faq")
            self.assertNotIn(EM_DASH, json_q)
            self.assertNotIn(EM_DASH, json_a)

        for url in collect_urls(payload):
            local_file = resolve_site_url(url)
            if local_file is not None:
                self.assertTrue(local_file.is_file(), f"JSON-LD site URL missing on disk: {url} -> {local_file}")

    def test_playground_and_dashboard_jsonld(self) -> None:
        _, pg = parse_page("playground.html")
        self.assertEqual(len(pg.jsonld_blocks), 1)
        pg_payload = json.loads(pg.jsonld_blocks[0])
        self.assertEqual(pg_payload.get("@context"), "https://schema.org")
        pg_by_type = {node.get("@type"): node for node in pg_payload.get("@graph", []) if isinstance(node, dict)}
        self.assertIn("WebApplication", pg_by_type)
        self.assertEqual(pg_by_type["WebApplication"].get("url"), f"{SITE_ORIGIN}playground.html")
        self.assertEqual(pg_by_type["WebApplication"].get("applicationCategory"), "DeveloperApplication")
        self.assertIn("BreadcrumbList", pg_by_type)
        pg_crumbs = pg_by_type["BreadcrumbList"].get("itemListElement", [])
        self.assertEqual([c.get("position") for c in pg_crumbs], [1, 2])
        self.assertEqual(pg_crumbs[0].get("item"), SITE_ORIGIN)
        self.assertEqual(pg_crumbs[1].get("item"), f"{SITE_ORIGIN}playground.html")

        _, dash = parse_page("dashboard.html")
        self.assertEqual(len(dash.jsonld_blocks), 1)
        dash_payload = json.loads(dash.jsonld_blocks[0])
        self.assertEqual(dash_payload.get("@context"), "https://schema.org")
        dash_by_type = {node.get("@type"): node for node in dash_payload.get("@graph", []) if isinstance(node, dict)}
        self.assertIn("Dataset", dash_by_type)
        ds = dash_by_type["Dataset"]
        self.assertEqual(ds.get("url"), f"{SITE_ORIGIN}dashboard.html")
        dist = ds.get("distribution", [])
        self.assertTrue(
            any(d.get("contentUrl") == f"{SITE_ORIGIN}data/latest.json" for d in dist if isinstance(d, dict)),
            "Dataset distribution must point to data/latest.json",
        )
        self.assertIn("BreadcrumbList", dash_by_type)
        dash_crumbs = dash_by_type["BreadcrumbList"].get("itemListElement", [])
        self.assertEqual([c.get("position") for c in dash_crumbs], [1, 2])
        self.assertEqual(dash_crumbs[0].get("item"), SITE_ORIGIN)
        self.assertEqual(dash_crumbs[1].get("item"), f"{SITE_ORIGIN}dashboard.html")

        for payload in (pg_payload, dash_payload):
            for url in collect_urls(payload):
                local_file = resolve_site_url(url)
                if local_file is not None:
                    self.assertTrue(local_file.is_file(), f"JSON-LD site URL missing on disk: {url} -> {local_file}")


if __name__ == "__main__":
    unittest.main()
