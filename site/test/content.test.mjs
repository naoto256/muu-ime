import assert from "node:assert/strict";
import test from "node:test";
import { readFileSync } from "node:fs";
import { navigation, markdown, pages, escape } from "../lib/content.js";
import { base, canonical, contentRef, repository } from "../lib/config.js";
import Page, { installCommand } from "../src/pages.11ty.js";

// One install command everywhere: a reader copying from the README, the book
// or the site gets the same HTTPS-only invocation.
test("the site, README and book use the same HTTPS-only latest install command", () => {
  const expected =
    "curl -q -fsSL --proto '=https' --proto-redir '=https' https://raw.githubusercontent.com/naoto256/muu-ime/main/install.sh | sh";
  const commands = [installCommand];
  for (const path of ["../../README.md", "../../docs/src/install.md"]) {
    const source = readFileSync(new URL(path, import.meta.url), "utf8");
    const matches = source.match(/^curl[^\n]*\/main\/install\.sh \| sh$/gm);
    assert.equal(matches?.length, 1, path);
    commands.push(matches[0]);
  }
  assert.deepEqual(commands, [expected, expected, expected]);
});

// Each rendered page carries the exact policy before any <link>, and its only
// script is the external copy module with no inline body, so script-src
// 'self' does not need an inline exception.
test("every page permits only local resources and the external copy module", () => {
  const renderer = new Page();
  for (const entry of pages()) {
    const html = renderer.render({ entry });
    const meta = html.match(
      /<meta http-equiv="Content-Security-Policy" content="([^"]+)">/,
    );
    assert.ok(meta, entry.route);
    const policy = new Map(
      meta[1]
        .split(";")
        .filter(Boolean)
        .map((part) => {
          const [name, ...values] = part.trim().split(/\s+/);
          return [name, values.join(" ")];
        }),
    );
    for (const name of [
      "default-src",
      "connect-src",
      "base-uri",
      "form-action",
      "object-src",
    ])
      assert.equal(policy.get(name), "'none'", name);
    for (const name of ["script-src", "style-src", "img-src"])
      assert.equal(policy.get(name), "'self'", name);
    assert.ok(html.indexOf(meta[0]) < html.indexOf("<link"));
    const scripts = [
      ...html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/g),
    ];
    assert.equal(scripts.length, 1);
    assert.ok(scripts[0][1].includes(`src="${base}copy-code.js"`));
    assert.equal(scripts[0][2], "");
  }
});

test("the book keeps every chapter in SUMMARY order", () => {
  const nav = navigation();
  assert.equal(nav.length, 17);
  assert.equal(nav[0].file, "introduction.md");
  assert.equal(nav[1].file, "install.md");
  assert.equal(nav.at(-1).file, "internals/licenses-and-limitations.md");
  const chapters = pages().filter((page) => page.kind === "docs");
  assert.deepEqual(
    chapters.map((page) => page.file),
    nav.map((item) => item.file),
  );
  assert.equal(chapters[1].previous.file, "introduction.md");
  assert.equal(chapters[1].next.file, "usage/input.md");
  assert.ok(chapters.every((chapter) => chapter.content.includes("<h1")));
});

test("navigation keeps groups and refuses missing, duplicate and escaping chapters", () => {
  assert.deepEqual(
    navigation(
      "# Summary\n[最初](./introduction.md)\n# 使い方\n- [入力](./usage/input.md)\n",
    ).map(({ group, file }) => ({ group, file })),
    [
      { group: "はじめに", file: "introduction.md" },
      { group: "使い方", file: "usage/input.md" },
    ],
  );
  assert.throws(() => navigation("# Summary"));
  assert.throws(() => navigation("[a](./a.md)\n[b](./a.md)"));
  assert.throws(() => navigation("[a](./../outside.md)"));
  assert.throws(() => navigation("[a](./a.txt)"));
});

test("Japanese and repeated heading anchors include inline code text", () => {
  const html = markdown(
    "# 日本語の見出し\n\n## 日本語の見出し\n\n## `コード` と見出し\n",
  );
  assert.ok(html.includes('id="日本語の見出し"'));
  assert.ok(html.includes('id="日本語の見出し-1"'));
  assert.ok(html.includes('id="コード-と見出し"'));
});

test("relative book links and repository sources resolve from the source chapter", () => {
  const html = markdown(
    "[章](./input.md#入力) [設計](../internals/architecture.md) [コード](../../../justfile) [外部](https://example.com/a?x=1&y=2)",
    "usage/about.md",
  );
  assert.ok(
    html.includes(
      `href="${base}docs/usage/input.html#${encodeURIComponent("入力")}"`,
    ),
  );
  assert.ok(html.includes(`href="${base}docs/internals/architecture.html"`));
  assert.ok(html.includes(`href="${repository}/blob/${contentRef}/justfile"`));
  assert.ok(html.includes('href="https://example.com/a?x=1&amp;y=2"'));
});

test("fenced bytes and tables survive while raw markup and script URLs remain inert", () => {
  const code = "  <hello> & 日本語\n\tline 2\n";
  const html = markdown(
    "```text\n" +
      code +
      "```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n<script>alert(1)</script>\n\n[bad](javascript:alert(1))",
  );
  assert.ok(html.includes("  &lt;hello&gt; &amp; 日本語\n\tline 2\n"));
  assert.ok(html.includes("<table>"));
  assert.doesNotMatch(html, /<script>|href="javascript:/);
});

test("every rendered page has Japanese language and project-local social metadata", () => {
  const all = pages();
  assert.equal(all.length, 19);
  const renderer = new Page();
  for (const entry of all) {
    const html = renderer.render({ entry });
    assert.ok(html.includes('<html lang="ja">'));
    assert.ok(
      html.includes(`rel="canonical" href="${canonical(entry.route)}"`),
    );
    assert.ok(
      html.includes(`property="og:image" content="${canonical("og.png")}"`),
    );
    assert.doesNotMatch(
      html,
      /PT Serif|fonts\.googleapis|google-site-verification/,
    );
  }
});

test("the home page and social artwork use the requested copy and exact install command", () => {
  const html = new Page().render({ entry: pages()[0] });
  const svg = readFileSync(
    new URL("../og-source.svg", import.meta.url),
    "utf8",
  );
  for (const text of [
    "Muu IME",
    "文字を、打ちたいように打つ！",
    "Moji wo Uchitai youni Utsu!",
    "macOS 用の日本語入力メソッド",
  ]) {
    assert.ok(html.includes(text));
    assert.ok(svg.includes(text));
  }
  assert.ok(html.includes(escape(installCommand)));
  assert.ok(html.includes("Apple の公証を受けていない配布です。"));
  assert.ok(html.includes(`${base}docs/install.html`));
  for (const text of [
    "端末の中で完結",
    "学習と辞書はあなたのもの",
    "通信は 2 つの場面だけ",
    "変換モデルで、もう一段賢く",
    "zenz を入れると、文全体を読んで第一候補を選び直します。入れなくても、辞書だけで変換できます。",
  ])
    assert.ok(html.includes(text));
  assert.doesNotMatch(
    html + svg,
    /変換は、あなたの Mac の中だけで。|日本語入力を、わたしたちの手に取り戻す。/,
  );
});
