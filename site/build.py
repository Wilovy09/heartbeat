# /// script
# requires-python = ">=3.10"
# dependencies = ["markdown>=3.5"]
# ///
"""Builds the Heartbeat site into site/dist: a landing page and the documentation, in
English (/en/) and Spanish (/es/), plus a root page that picks the visitor's language.

The docs are README.md and README_ES.md themselves, so they never drift from the repo.
Colors come from the app's own static/tokens.css (system, light and dark themes).

    uv run site/build.py                      # build
    uv run site/build.py --serve              # build and serve on http://localhost:8200
    SITE_URL=https://example.com uv run site/build.py
"""

import html
import http.server
import json
import os
import re
import shutil
import sys
from functools import partial
from pathlib import Path

import markdown

ROOT = Path(__file__).resolve().parent.parent
SITE = ROOT / "site"
OUT = SITE / "dist"
REPO = "https://github.com/Wilovy09/heartbeat"
SITE_URL = os.environ.get("SITE_URL", "https://wilovy09.github.io/heartbeat").rstrip("/")
LANGS = {"en": "README.md", "es": "README_ES.md"}
# README sections the site shows its own way (the landing has a screenshot gallery).
SKIPPED_SECTIONS = {"Screenshots", "Capturas"}
PLACEHOLDER = re.compile(r"\{\{\s*([\w.]+)\s*\}\}")


def load_catalogs() -> dict[str, dict[str, str]]:
    catalogs = {
        lang: json.loads((SITE / "i18n" / f"{lang}.json").read_text()) for lang in LANGS
    }
    keys = [set(c) for c in catalogs.values()]
    if any(k != keys[0] for k in keys):
        diff = set.union(*keys) - set.intersection(*keys)
        sys.exit(f"i18n catalogs don't have the same keys: {sorted(diff)}")
    return catalogs


def render(template: str, ctx: dict[str, str]) -> str:
    """Fills {{ key }} placeholders; a missing key is an error, not a blank."""

    def value(match: re.Match) -> str:
        key = match.group(1)
        if key not in ctx:
            raise KeyError(f"template placeholder {{{{ {key} }}}} has no value")
        return ctx[key]

    return PLACEHOLDER.sub(value, template)


def context(catalog: dict[str, str], lang: str, prefix: str, page: str) -> dict[str, str]:
    """Template values for one page: catalog strings (escaped, unless the key ends in
    `_html`), plus paths relative to the page so the site works under any base path."""
    ctx = {}
    for key, text in catalog.items():
        text = text.replace("{repo}", REPO)
        ctx[key] = text if key.endswith("_html") else html.escape(text)
    other = next(l for l in LANGS if l != lang)
    ctx.update(
        {
            "lang": lang,
            "root": prefix,
            "assets": f"{prefix}assets",
            "home": f"{prefix}{lang}/",
            "docs_url": f"{prefix}{lang}/docs/",
            "other_lang_url": f"{prefix}{other}/{page}",
            "canonical": f"{SITE_URL}/{lang}/{page}",
            "alt_en": f"{SITE_URL}/en/{page}",
            "alt_es": f"{SITE_URL}/es/{page}",
            "repo": REPO,
        }
    )
    return ctx


def ticker(catalog: dict[str, str]) -> str:
    items = "".join(
        f"<li>{html.escape(item)}</li>" for item in catalog["ticker.items"].split("|")
    )
    # Two copies side by side make the loop seamless; the second is hidden from readers.
    return f'<ul class="ticker-track">{items}</ul><ul class="ticker-track" aria-hidden="true">{items}</ul>'


def readme_sections(lang: str) -> str:
    """The README as docs markdown: without the title, the language switcher and the
    sections the site shows elsewhere."""
    text = (ROOT / LANGS[lang]).read_text()
    parts = re.split(r"(?m)^## ", text)
    preamble = "\n".join(
        line
        for line in parts[0].splitlines()
        if not line.startswith("# ") and "README" not in line
    )
    kept = [p for p in parts[1:] if p.splitlines()[0].strip() not in SKIPPED_SECTIONS]
    return preamble + "".join("\n## " + p for p in kept)


def rewrite_links(body: str, lang: str) -> str:
    """Repo-relative links in the README point at files GitHub serves; on the site they
    become the copied assets, the other language's docs, or links into the repo."""

    def fix(match: re.Match) -> str:
        attr, url = match.group(1), match.group(2)
        if url.startswith(("http://", "https://", "#", "mailto:")):
            return match.group(0)
        if url.startswith(".github/public/badges/"):
            url = "../../assets/badges/" + url.rsplit("/", 1)[1]
        elif url.startswith(".github/public/"):
            url = "../../assets/screens/" + url.rsplit("/", 1)[1]
        elif url in LANGS.values():
            target = next(l for l, f in LANGS.items() if f == url)
            url = f"../../{target}/docs/"
        else:
            url = f"{REPO}/blob/main/{url}"
        return f'{attr}="{url}"'

    return re.sub(r'(href|src)="([^"]+)"', fix, body)


def toc_html(tokens: list[dict]) -> str:
    if not tokens:
        return ""
    items = []
    for token in tokens:
        children = toc_html(token.get("children", []))
        items.append(
            f'<li><a href="#{token["id"]}">{html.escape(html.unescape(token["name"]))}</a>{children}</li>'
        )
    return "<ul>" + "".join(items) + "</ul>"


def build_docs(lang: str) -> tuple[str, str]:
    md = markdown.Markdown(
        extensions=["fenced_code", "tables", "toc"],
        extension_configs={"toc": {"toc_depth": "2-3", "permalink": False}},
    )
    body = rewrite_links(md.convert(readme_sections(lang)), lang)
    # Wide tables scroll inside their own box instead of widening the page.
    body = body.replace("<table>", '<div class="table-wrap"><table>').replace(
        "</table>", "</table></div>"
    )
    return body, toc_html(md.toc_tokens)


def copy_assets() -> None:
    assets = OUT / "assets"
    shutil.copytree(SITE / "assets", assets)
    shutil.copy(ROOT / "static" / "tokens.css", assets / "tokens.css")
    shutil.copy(ROOT / "static" / "icon.svg", assets / "icon.svg")
    fonts = assets / "fonts"
    fonts.mkdir()
    for font in (ROOT / "static" / "fonts").glob("ibm-plex-mono-*.woff2"):
        shutil.copy(font, fonts / font.name)
    screens = assets / "screens"
    screens.mkdir()
    for image in (ROOT / ".github" / "public").glob("*.png"):
        shutil.copy(image, screens / image.name)
    shutil.copytree(ROOT / ".github" / "public" / "badges", assets / "badges")


def build() -> None:
    catalogs = load_catalogs()
    templates = {
        name: (SITE / "templates" / f"{name}.html").read_text()
        for name in ("head", "nav", "footer", "landing", "docs", "redirect")
    }
    if OUT.exists():
        shutil.rmtree(OUT)
    OUT.mkdir()
    copy_assets()

    for lang, catalog in catalogs.items():
        for page, prefix, template in (("", "../", "landing"), ("docs/", "../../", "docs")):
            ctx = context(catalog, lang, prefix, page)
            ctx["ticker_html"] = ticker(catalog)
            if template == "docs":
                ctx["docs_html"], ctx["toc_html"] = build_docs(lang)
                ctx["page_title"] = ctx["docs.meta.title"]
                ctx["edit_url"] = f"{REPO}/blob/main/{LANGS[lang]}"
            else:
                ctx["page_title"] = ctx["meta.title"]
            for part in ("head", "nav", "footer"):
                ctx[f"{part}_html"] = render(templates[part], ctx)
            target = OUT / lang / page / "index.html"
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(render(templates[template], ctx))

    ctx = context(catalogs["en"], "en", "", "")
    ctx["es_url"], ctx["en_url"] = "es/", "en/"
    ctx["page_title"] = ctx["redirect.title"]
    ctx["head_html"] = render(templates["head"], ctx)
    (OUT / "index.html").write_text(render(templates["redirect"], ctx))
    (OUT / ".nojekyll").write_text("")
    print(f"site built in {OUT.relative_to(ROOT)} for {SITE_URL}")


def serve(port: int = 8200) -> None:
    handler = partial(http.server.SimpleHTTPRequestHandler, directory=str(OUT))
    print(f"serving on http://localhost:{port}/")
    http.server.ThreadingHTTPServer(("", port), handler).serve_forever()


if __name__ == "__main__":
    build()
    if "--serve" in sys.argv:
        serve()
