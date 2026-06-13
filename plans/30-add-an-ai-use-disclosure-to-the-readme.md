# Plan — #30 Add an "AI use" disclosure to the README

## Goal

Add a short, factual **"AI use"** disclosure to `README.md` stating that this
project is developed with Claude Code (Anthropic), and explaining the
traceability that the ghwf workflow gives from inputs (prompts) to outputs
(code/docs).

## Decisions locked at pre-plan (approved 👍)

1. **README only** — no new docs pages elsewhere.
2. **Placement**: a new top-level `## AI use` section appended at the end of the
   README, after the existing **Status** section.
3. **Tone**: brief and factual, matching the README's concise style — a short
   intro sentence plus a couple of bullets.
4. **Content** covers:
   - Developed with **Claude Code** (Anthropic's CLI agent).
   - Driven via **ghwf**, giving natural traceability from prompts to outputs.
   - Prompts/conversations are, for the most part, captured in the GitHub
     **issue / PR comment threads**.
   - **Plan files** are committed to the git tree (in `plans/`).

## Changes

### `README.md`

Append one new section after **Status**:

```markdown
## AI use

This project is developed with **Claude Code** (Anthropic's CLI coding agent),
driven through the [ghwf](https://github.com/...) GitHub workflow. Because of
that workflow, there is natural traceability from the inputs (prompts) to the
outputs (code, docs, and tests):

- The prompts and conversations that produced the work are, for the most part,
  captured in the **issue and pull-request comment threads** on GitHub.
- The **implementation plans** Claude writes for each issue are committed to the
  git tree under [`plans/`](plans/).
```

Exact wording will be finalised during implementation; the ghwf link target will
be confirmed (or the link dropped if there's no canonical public URL) so we don't
ship a broken link.

## Out of scope

- Any change to behaviour, code, or other docs.
- A dedicated AI-use / governance doc under `docs/` (can be a follow-up if the
  disclosure ever needs to grow).

## Verification

- Re-read the rendered section for accuracy and tone.
- Confirm any link in the new section resolves (no broken links).
