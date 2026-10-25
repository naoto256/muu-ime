import { execFileSync } from "node:child_process";

export const base = process.env.SITE_BASE || "/muu-ime/";
if (!/^\/(?:[a-zA-Z0-9_-]+\/)*$/.test(base))
  throw new Error("SITE_BASE must be an absolute directory path ending in /");
export const url = (path = "") => base + path.replace(/^\//, "");
export const repository = "https://github.com/naoto256/muu-ime";
export const origin = process.env.SITE_ORIGIN || "https://naoto256.github.io";
if (new URL(origin).origin !== origin || !origin.startsWith("https://"))
  throw new Error(
    "SITE_ORIGIN must be an HTTPS origin without a trailing slash",
  );
export const canonical = (path) =>
  origin + url(path.replace(/index\.html$/, ""));
export const contentRef = execFileSync("git", ["rev-parse", "HEAD"], {
  cwd: new URL("../../", import.meta.url),
  encoding: "utf8",
}).trim();
