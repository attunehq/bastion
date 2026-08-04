# Bastion marketing site

The source for [bastion.attune.inc](https://bastion.attune.inc): the
single-page explainer for what Bastion is and why you'd want it, plus the hosted
**user guide** under `/guide`.

It is a static [Astro](https://astro.build) site. The Rust crate in the repository
root is untouched by it; the Node toolchain is scoped entirely to this directory.

## Develop

```sh
cd site
npm install
npm run dev      # http://localhost:4321
```

```sh
npm run build    # static output to site/dist (also builds the Pagefind search index)
npm run preview  # serve the production build locally
npm run check    # astro type/diagnostics check
```

Search is powered by [Pagefind](https://pagefind.app), whose index is generated
from the built HTML as a post-build step. It therefore only exists after
`npm run build`; under `npm run dev` the search box opens but reports that the
index has not been built yet.

## Structure

### Marketing page

- `src/pages/index.astro` composes the page from the section components.
- `src/components/` holds one component per section (`Hero`, `Problem`,
  `Reviewers`, `Gate`, `Govern`, `Mirror`, `Trust`, `Install`) plus shared atoms
  (`Nav`, `Footer`, `SectionHeading`, `Artifact`, `Logo`).

### User guide (`/guide`)

The guide is single-sourced from the repo-root `docs/user-guide` markdown (the
same files people read on GitHub). The site renders those files in place rather
than copying them, so the docs never drift between surfaces.

- `src/content.config.ts` defines the `guide` content collection with a glob
  loader over `../docs/user-guide`. Each chapter carries `title`, `summary`, and
  `order` frontmatter that drives the sidebar, page metadata, and the llms.txt
  index.
- `src/lib/rehype-doc-links.mjs` (wired in `astro.config.mjs`) rewrites the
  guide's relative `.md` links at build time: links inside the guide become
  `/guide/*` routes, and links pointing elsewhere (developer guide, registry,
  root files) become GitHub URLs. This is what lets one set of files serve both
  GitHub and the site.
- `src/layouts/Docs.astro` is the docs shell; `src/components/docs/` holds the
  header, sidebar, on-this-page TOC, prev/next, and per-page actions.
- `src/pages/guide/index.astro` and `src/pages/guide/[slug].astro` render the
  chapters; `src/styles/docs.css` styles the prose against the shared tokens.

### Agent / LLM surface

- `src/pages/guide/[slug].md.ts` serves every page's raw Markdown at a
  predictable `.md` URL (e.g. `/guide/concepts.md`): the representation agents
  probe for and the text the "Copy page" button copies.
- `src/pages/llms.txt.ts` and `src/pages/llms-full.txt.ts` generate
  [`/llms.txt`](https://llmstxt.org) (a curated index) and `/llms-full.txt` (the
  whole guide concatenated for a single fetch).

### Shared

- `src/styles/tokens.css` is the design system: colors (OKLCH), type scale,
  spacing, motion. `src/styles/global.css` is the reset, base type, and shared
  utilities.
- `public/` holds static assets: `CNAME` (custom domain), `favicon.svg`, and
  `og.png` (the Open Graph card).

Design intent and the visual system are documented in `PRODUCT.md` and `DESIGN.md`.

## The Open Graph image

`public/og.png` is generated from an on-brand template. To regenerate it (for
example after changing the headline), install Playwright once and run the script:

```sh
npm install --no-save playwright && npx playwright install chromium
node scripts/og-gen.mjs
```

Playwright is intentionally not a tracked dependency: the committed `og.png` is the
source of truth, and the generator is only needed when it changes.

## Deploying

Pushes to `main` that touch `site/**` trigger
[`.github/workflows/site.yml`](../.github/workflows/site.yml), which builds the site
and deploys it to GitHub Pages.

A custom domain needs three things set up once, outside this repo:

1. **DNS:** a `CNAME` record for `bastion.attune.inc` pointing at
   `attunehq.github.io` (or four `A` records to the GitHub Pages apex IPs if you
   ever serve a bare apex instead of a subdomain).
2. **Repo settings -> Pages:** set the build source to **GitHub Actions**, and set
   the custom domain to `bastion.attune.inc` (this matches `public/CNAME`, which
   the build copies into `dist/`). Enable **Enforce HTTPS** once the certificate is
   issued.
3. The first successful run of the workflow publishes the site.
