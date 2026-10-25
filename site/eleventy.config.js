import { existsSync } from "node:fs";
import { pages } from "./lib/content.js";
import { base } from "./lib/config.js";

export default function (config) {
  config.addGlobalData("pages", pages);
  config.addWatchTarget("../docs/src/");
  config.addWatchTarget("../apple/MuuIME/ModeIconArtwork/MuuComponent.svg");
  config.addPassthroughCopy({
    "src/style.css": "style.css",
    "src/copy-code.js": "copy-code.js",
    "src/og.png": "og.png",
    "../apple/MuuIME/ModeIconArtwork/MuuComponent.svg": "mark.svg",
  });
  if (existsSync("../docs/src/assets"))
    config.addPassthroughCopy({ "../docs/src/assets": "docs/assets" });
  return {
    dir: { input: "src", output: "dist" },
    pathPrefix: base,
    templateFormats: ["11ty.js"],
  };
}
