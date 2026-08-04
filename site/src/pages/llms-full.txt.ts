import type { APIRoute } from "astro";
import { loadGuide, rawSlugFor } from "../lib/guide";

// llms-full.txt: the entire user guide concatenated in reading order, so an
// agent can ingest the whole thing in one request. Each section is prefixed
// with its canonical URL for citation.
const SITE = "https://bastion.attune.inc";

export const GET: APIRoute = async () => {
  const entries = await loadGuide();

  const sections = entries.map((e) => {
    const url =
      rawSlugFor(e) === "index" ? `${SITE}/guide` : `${SITE}/guide/${rawSlugFor(e)}`;
    return `<!-- ${url} -->\n\n${(e.body ?? "").trim()}\n`;
  });

  const body =
    "# Bastion user guide (full)\n\n> The complete user guide, concatenated in reading order. Canonical pages live under " +
    `${SITE}/guide.\n\n` +
    sections.join("\n---\n\n");

  return new Response(body, {
    headers: { "Content-Type": "text/plain; charset=utf-8" },
  });
};
