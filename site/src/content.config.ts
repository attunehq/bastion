import { defineCollection } from "astro:content";
import { glob } from "astro/loaders";
import { z } from "astro/zod";

// The user guide is single-sourced from the repo-root `docs/user-guide`
// markdown (the same files people read on GitHub and that AGENTS.md treats as a
// source of truth). The site renders those files in place rather than copying
// them, so the docs never drift between surfaces. Frontmatter (title, summary,
// order) drives the sidebar, page metadata, and the llms.txt index.
const guide = defineCollection({
  loader: glob({ pattern: "**/*.md", base: "../docs/user-guide" }),
  schema: z.object({
    title: z.string(),
    summary: z.string(),
    order: z.number(),
  }),
});

export const collections = { guide };
