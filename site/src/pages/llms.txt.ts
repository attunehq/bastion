import type { APIRoute } from "astro";
import { loadGuide, isIndex, rawSlugFor } from "../lib/guide";

// llms.txt (https://llmstxt.org): a curated, machine-readable index of the
// docs so an agent can discover the guide and fetch any page as Markdown in one
// hop. Each link points at the raw `.md` representation served from /guide.
const SITE = "https://bastion.attune.inc";

export const GET: APIRoute = async () => {
  const entries = await loadGuide();
  const index = entries.find(isIndex);
  const chapters = entries.filter((e) => !isIndex(e));

  const out: string[] = [];
  out.push("# Bastion");
  out.push("");
  out.push(
    "> Agentic code review built as single-concern reviewers: declarative, human-authored fitness functions over every changeset that aggregate into one merge gate that fails closed."
  );
  out.push("");
  out.push(
    "Bastion is a Rust CLI and GitHub CI adapter. You declare reviewers in `.bastion.yaml`; matched reviewers run over a changeset and return structured verdicts that Bastion aggregates into a single merge decision. Humans govern the reviewer policy rather than reviewing every diff."
  );
  out.push("");
  out.push("## User guide");
  out.push("");
  if (index) {
    out.push(`- [${index.data.title}](${SITE}/guide/index.md): ${index.data.summary}`);
  }
  for (const e of chapters) {
    out.push(`- [${e.data.title}](${SITE}/guide/${rawSlugFor(e)}.md): ${e.data.summary}`);
  }
  out.push("");
  out.push("## Optional");
  out.push("");
  out.push(`- [Full guide as one file](${SITE}/llms-full.txt): every chapter concatenated for a single fetch.`);
  out.push(`- [Source repository](https://github.com/attunehq/bastion): the developer guide, reviewer registry, and code.`);
  out.push("");

  return new Response(out.join("\n"), {
    headers: { "Content-Type": "text/plain; charset=utf-8" },
  });
};
