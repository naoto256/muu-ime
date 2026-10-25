import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { base } from "./lib/config.js";

const root = path.resolve("dist");
const types = {
  ".html": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".png": "image/png",
  ".svg": "image/svg+xml",
};
createServer(async (req, res) => {
  if (!["GET", "HEAD"].includes(req.method)) {
    res.writeHead(405, { Allow: "GET, HEAD" }).end();
    return;
  }
  try {
    const pathname = decodeURIComponent(
      new URL(req.url, "http://127.0.0.1").pathname,
    );
    if (pathname === "/" && base !== "/") {
      res.writeHead(302, { Location: base }).end();
      return;
    }
    if (!pathname.startsWith(base)) {
      res.writeHead(404).end();
      return;
    }
    const relative =
      pathname.slice(base.length) +
      (pathname.endsWith("/") ? "index.html" : "");
    const file = path.resolve(root, relative);
    if (!file.startsWith(root + path.sep)) {
      res.writeHead(404).end();
      return;
    }
    const data = await readFile(file);
    res
      .writeHead(200, {
        "Content-Type": types[path.extname(file)] || "application/octet-stream",
      })
      .end(req.method === "HEAD" ? undefined : data);
  } catch {
    res.writeHead(404).end("Not found");
  }
}).listen(8090, "127.0.0.1", () =>
  console.log(`Production preview: http://127.0.0.1:8090${base}`),
);
