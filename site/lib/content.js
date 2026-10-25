import { readFileSync } from "node:fs";
import path from "node:path";
import MarkdownIt from "markdown-it";
import hljs from "highlight.js/lib/core";
import bash from "highlight.js/lib/languages/bash";
import json from "highlight.js/lib/languages/json";
import { contentRef, repository, url } from "./config.js";

hljs.registerLanguage("bash", bash);
hljs.registerLanguage("sh", bash);
hljs.registerLanguage("json", json);
export const read = (file) =>
  readFileSync(new URL("../../" + file, import.meta.url), "utf8");
export const escape = (text) =>
  String(text).replace(
    /[&<>"']/g,
    (c) =>
      ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[
        c
      ],
  );
export const route = (file) =>
  "docs/" +
  file.replace(/\.md$/, ".html").replace(/(^|\/)README\.html$/, "$1index.html");
export const slug = (text) =>
  text
    .toLowerCase()
    .replace(/[^\p{L}\p{N}_\-\s]/gu, "")
    .replace(/\s/g, "-");

export function markdown(source, file = "introduction.md") {
  const md = new MarkdownIt({
    html: false,
    linkify: false,
    highlight(code, language) {
      return hljs.getLanguage(language)
        ? hljs.highlight(code, { language, ignoreIllegals: true }).value
        : "";
    },
  });
  const counts = new Map();
  md.renderer.rules.heading_open = (tokens, index, options, env, renderer) => {
    const plain = tokens[index + 1].children
      .map((token) =>
        token.type === "image" ||
        token.type === "text" ||
        token.type.endsWith("_inline")
          ? token.content
          : "",
      )
      .join("");
    const key = slug(plain);
    const count = counts.get(key) || 0;
    counts.set(key, count + 1);
    tokens[index].attrSet("id", key + (count ? `-${count}` : ""));
    return renderer.renderToken(tokens, index, options);
  };
  function resolve(href) {
    if (href.startsWith("#") || /^(?:[a-z]+:|\/\/)/i.test(href)) return href;
    const [pathname, fragment] = href.split("#");
    const target = path.posix.normalize(
      path.posix.join(path.posix.dirname(file), pathname),
    );
    const suffix = fragment ? "#" + fragment : "";
    if (target.startsWith("../")) {
      const repoPath = path.posix.normalize(
        path.posix.join("docs/src", target),
      );
      if (repoPath.startsWith("../"))
        throw new Error("Link escapes repository: " + href);
      return `${repository}/blob/${contentRef}/${repoPath}${suffix}`;
    }
    return (
      url(target.endsWith(".md") ? route(target) : `docs/${target}`) + suffix
    );
  }
  md.renderer.rules.link_open = (tokens, index, options, env, renderer) => {
    tokens[index].attrSet("href", resolve(tokens[index].attrGet("href")));
    return renderer.renderToken(tokens, index, options);
  };
  const image = md.renderer.rules.image;
  md.renderer.rules.image = (tokens, index, options, env, renderer) => {
    tokens[index].attrSet("src", resolve(tokens[index].attrGet("src")));
    tokens[index].attrSet("loading", "lazy");
    return image(tokens, index, options, env, renderer);
  };
  return md.render(source);
}

export function navigation(source = read("docs/src/SUMMARY.md")) {
  const items = [];
  let group = "はじめに";
  for (const line of source.split("\n")) {
    if (line.startsWith("# ") && line !== "# Summary") group = line.slice(2);
    const match = line.match(/^(\s*)(?:- )?\[([^\]]+)\]\(\.\/([^)]*)\)/);
    if (!match) continue;
    const file = match[3];
    if (
      !file.endsWith(".md") ||
      file.startsWith("/") ||
      file.split("/").includes("..")
    )
      throw new Error("Invalid chapter path: " + file);
    if (items.some((item) => item.file === file))
      throw new Error("Duplicate chapter: " + file);
    items.push({ title: match[2], file, group, route: route(file) });
  }
  if (!items.length) throw new Error("SUMMARY.md must contain chapters");
  return items;
}

export function pages() {
  const nav = navigation();
  return [
    { kind: "home", title: "Muu IME", route: "index.html" },
    { kind: "index", title: "ドキュメント", route: "docs/index.html", nav },
    ...nav.map((item, index) => ({
      ...item,
      kind: "docs",
      content: markdown(read(`docs/src/${item.file}`), item.file),
      nav,
      previous: nav[index - 1],
      next: nav[index + 1],
    })),
  ];
}
