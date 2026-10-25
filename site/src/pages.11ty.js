import { canonical, contentRef, repository, url } from "../lib/config.js";
import { escape } from "../lib/content.js";

// The one install command shown on the site; README and the book carry the
// same text (a test compares them). -q ignores a user's .curlrc, and both
// --proto flags keep the first request and every redirect on HTTPS.
export const installCommand =
  "curl -q -fsSL --proto '=https' --proto-redir '=https' https://raw.githubusercontent.com/naoto256/muu-ime/main/install.sh | sh";
// Delivered as a meta tag because GitHub Pages cannot set response headers;
// it precedes every <link> and <script> so it governs all of them.
// default-src 'none': anything not listed below is refused.
// script-src 'self': only the site's own copy-code.js module runs; inline or
// injected script in rendered Markdown does not.
// style-src 'self' and img-src 'self': the site's stylesheet and images only,
// with no inline styles and no images from other origins.
// connect-src 'none': pages make no fetch, XHR or WebSocket requests.
// base-uri 'none': an injected <base> cannot redirect relative links.
// form-action 'none': there are no forms, so nothing can submit anywhere.
// object-src 'none': no plugins or embedded objects.
const contentSecurityPolicy =
  "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'none'; base-uri 'none'; form-action 'none'; object-src 'none'";
const arrow = '<span aria-hidden="true">↗</span>';

function groupedNavigation(items, current) {
  let group;
  return items
    .map((item) => {
      const heading =
        group !== item.group ? `<h2>${escape(item.group)}</h2>` : "";
      group = item.group;
      return `${heading}<a href="${url(item.route)}"${item.route === current ? ' aria-current="page"' : ""}>${escape(item.title)}</a>`;
    })
    .join("");
}

function home() {
  const features = [
    ["端末の中で完結", "読みも候補も、ネットワークへ送りません。"],
    [
      "学習と辞書はあなたのもの",
      "保存するのは語の番号と回数だけ。いつでも消せます。",
    ],
    [
      "通信は 2 つの場面だけ",
      "郵便番号の更新と、変換モデルの取得。どちらもあなたの操作で始まります。",
    ],
    [
      "変換モデルで、もう一段賢く",
      "zenz を入れると、文全体を読んで第一候補を選び直します。入れなくても、辞書だけで変換できます。",
    ],
  ];
  return `<main id="main" class="home">
    <section class="hero" aria-labelledby="product-name">
      <img class="hero-mark" src="${url("mark.svg")}" width="88" height="88" alt="">
      <h1 id="product-name">Muu IME</h1>
      <p class="hero-copy">文字を、打ちたいように打つ！</p>
      <p class="hero-romaji" lang="en">Moji wo Uchitai youni Utsu!</p>
      <p class="hero-sub">macOS 用の日本語入力メソッド</p>
      <div class="hero-actions"><a class="button primary" href="#install">インストール <span aria-hidden="true">↓</span></a><a class="text-link" href="${url("docs/")}">ドキュメントを読む <span aria-hidden="true">→</span></a></div>
      <p class="requirements">macOS 15 以降 <span aria-hidden="true">·</span> Apple Silicon</p>
    </section>
    <section class="features" aria-label="Muu IME の特徴">${features.map(([title, body], index) => `<div class="feature"><span class="feature-number" aria-hidden="true">0${index + 1}</span><div><h2>${title}</h2><p>${body}</p></div></div>`).join("")}</section>
    <section class="installation" id="install" aria-labelledby="install-heading">
      <p class="eyebrow">インストール</p><h2 id="install-heading">この 1 行から。</h2>
      <div class="terminal"><div class="terminal-heading"><span>ターミナル</span><span>macOS · arm64</span></div><pre><code class="language-sh">${escape(installCommand)}</code></pre></div>
      <p class="install-note">Apple の公証を受けていない配布です。何を確かめてから入れるかは<a href="${url("docs/install.html")}">インストールと更新</a>に書いています。</p>
    </section>
    <section class="read-more"><div><h2>ドキュメントを読む</h2><p>インストール、日々の使い方、Muu IME の仕組み。</p></div><a class="round-link" href="${url("docs/")}" aria-label="ドキュメントを読む"><span aria-hidden="true">→</span></a></section>
  </main>`;
}

function documentation(entry) {
  const content =
    entry.kind === "index"
      ? `<p class="eyebrow">Muu IME</p><h1>ドキュメント</h1><p class="chapter-lead">インストールから、毎日の使い方、仕組みまで。</p><div class="chapter-index">${groupedNavigation(entry.nav)}</div>`
      : `<a class="breadcrumb" href="${url("docs/")}">ドキュメント</a>${entry.content}<nav class="chapter-pagination" aria-label="前後の章">${entry.previous ? `<a href="${url(entry.previous.route)}"><span>前の章</span>← ${escape(entry.previous.title)}</a>` : "<span></span>"}${entry.next ? `<a href="${url(entry.next.route)}"><span>次の章</span>${escape(entry.next.title)} →</a>` : ""}</nav><p class="source-link"><a href="${repository}/blob/${contentRef}/docs/src/${entry.file}">このページのソース ${arrow}</a></p>`;
  return `<div class="docs-layout"><aside class="sidebar"><details open><summary>目次</summary><nav aria-label="本の目次">${groupedNavigation(entry.nav, entry.route)}</nav></details></aside><main id="main" class="prose">${content}</main></div>`;
}

export default class {
  data() {
    return {
      pagination: { data: "pages", size: 1, alias: "entry" },
      permalink: ({ entry }) => entry.route,
      eleventyExcludeFromCollections: true,
    };
  }
  render({ entry }) {
    const title =
      entry.kind === "home"
        ? "Muu IME — 文字を、打ちたいように打つ！"
        : `${entry.title} — Muu IME`;
    const description =
      "文字を、打ちたいように打つ！ Moji wo Uchitai youni Utsu! macOS 用の日本語入力メソッド";
    const image = canonical("og.png");
    return `<!doctype html><html lang="ja"><head><meta charset="utf-8"><meta http-equiv="Content-Security-Policy" content="${contentSecurityPolicy}"><meta name="viewport" content="width=device-width, initial-scale=1"><title>${escape(title)}</title><meta name="description" content="${description}"><meta name="theme-color" content="#faf8f4"><link rel="canonical" href="${escape(canonical(entry.route))}"><meta property="og:locale" content="ja_JP"><meta property="og:title" content="${escape(title)}"><meta property="og:type" content="website"><meta property="og:url" content="${escape(canonical(entry.route))}"><meta property="og:description" content="${description}"><meta property="og:site_name" content="Muu IME"><meta property="og:image" content="${image}"><meta property="og:image:width" content="1200"><meta property="og:image:height" content="630"><meta property="og:image:type" content="image/png"><meta property="og:image:alt" content="Muu IME — ${description}"><meta name="twitter:card" content="summary_large_image"><meta name="twitter:title" content="${escape(title)}"><meta name="twitter:description" content="${description}"><meta name="twitter:image" content="${image}"><meta name="twitter:image:alt" content="Muu IME — ${description}"><link rel="icon" type="image/svg+xml" href="${url("mark.svg")}"><link rel="stylesheet" href="${url("style.css")}"><script type="module" src="${url("copy-code.js")}"></script></head><body><a class="skip" href="#main">本文へ移動</a><header class="site-header"><a class="brand" href="${url()}"><img src="${url("mark.svg")}" width="28" height="28" alt=""><span>Muu IME</span></a><nav aria-label="メイン"><a href="${url("docs/")}"${entry.kind !== "home" ? ' aria-current="true"' : ""}>ドキュメント</a><a href="${repository}">GitHub ${arrow}</a></nav></header>${entry.kind === "home" ? home() : documentation(entry)}<footer class="site-footer"><a class="brand" href="${url()}"><img src="${url("mark.svg")}" width="22" height="22" alt=""><span>Muu IME</span></a><div><a href="${url("docs/")}">ドキュメント</a><a href="${repository}">GitHub ${arrow}</a><a href="${repository}/blob/${contentRef}/LICENSE">MIT License ${arrow}</a></div></footer></body></html>`;
  }
}
