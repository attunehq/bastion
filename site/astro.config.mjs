// @ts-check
import { defineConfig } from "astro/config";
import { unified } from "@astrojs/markdown-remark";
import rehypeDocLinks from "./src/lib/rehype-doc-links.mjs";

// Custom domain (CNAME in public/) serves the site at the root path, so no
// `base` is needed. `site` powers canonical URLs, sitemap, and Open Graph tags.
export default defineConfig({
  site: "https://bastion.attune.inc",
  trailingSlash: "never",
  build: {
    inlineStylesheets: "auto",
  },
  markdown: {
    // Rewrite the guide's relative `.md` links to site routes / GitHub URLs.
    // Astro 6 takes remark/rehype plugins through a `unified()` processor.
    processor: unified({ rehypePlugins: [rehypeDocLinks] }),
    // A light syntax theme that sits in the drafting-paper surface rather than
    // dropping a dark slab into the page. Code-block chrome is styled in the
    // docs prose CSS.
    shikiConfig: {
      theme: "github-light",
      wrap: false,
    },
  },
});
