# @gleaph/web-lp

The public landing page for Gleaph, served as a static Astro site.

The page keeps the section order of [CUI's overview page](https://yappologistic.github.io/CUI/#/):
a hero over a photograph printed as an ordered halftone, a prepared-query stage, a tagline, a
query walkthrough and a closing CTA. The hero is a split, not CUI's centred stack: copy on the
left, the query stage on the right, both starting on one line. It is written against the
`landing-page-design` and `frontend-design` skills and uses the Gleaph design tokens, not CUI's
palette.

## Commands

```bash
# from this directory (the local binaries avoid a workspace-wide install check)
./node_modules/.bin/astro dev
./node_modules/.bin/astro build
./node_modules/.bin/astro preview
```

`astro check` currently cannot run in this repository: the workspace pins TypeScript
7.0.2, and `@astrojs/language-server` still needs the programmatic API that TypeScript 7
removed. Type-check the hand-written modules directly instead (`--ignoreConfig` is needed
because the project tsconfig is not consumed when files are listed):

```bash
./node_modules/.bin/tsc --ignoreConfig --noEmit --strict --target es2022 --module esnext \
  --moduleResolution bundler --lib es2022,dom,dom.iterable src/lib/dither.ts src/lib/icons.ts
```

## Files

| Path | Role |
| --- | --- |
| `src/pages/index.astro` | The page: hero with the query stage beside it, capabilities, tagline, language, query steps, facts, guides, FAQ, CTA, footer, and the client script (theme, tabs, reveals, tagline, copy, mobile menu). |
| `src/pages/404.astro` | Branded not-found page. |
| `src/layouts/Layout.astro` | Document shell, canonical and absolute social metadata from `site`, and the pre-paint theme and JS flags. |
| `src/styles/global.css` | Tokens, named text styles, layout, motion, responsive rules. |
| `src/lib/dither.ts` | The halftone art engine (framework agnostic, no dependencies). |
| `src/lib/icons.ts`, `src/components/Icon.astro` | Inline icon set. |
| `public/art/dunes.jpg` | The hero photograph, grayscale, used only as luminance. |
| `public/og.jpg` | 1200×630 social preview captured from the page. |
| `public/robots.txt` | Allow-all crawl policy (no sitemap integration yet). |
| `public/fonts/` | Self-hosted Geist and Geist Mono (SIL Open Font License, see the license file). |

## Design system

- **One family.** Geist sets every piece of text; Geist Mono is reserved for GQL, file
  names, numeric values and the labels inside the technical diagrams, where character
  alignment carries meaning.
- **Named text styles.** `heading-1` … `heading-4`, `title-1` … `title-4`, `body-3`, `body-6`,
  `body-7`, `caption-1`, `caption-2`, `caption-4`, `caption-5`, `button-md`, `mono-data`.
  Sizes snap to Tailwind's scale steps and are defined once in `global.css`.
- **Spacing and radii** come from the landing-page scale (2/4/8/12/16/24/32/40/48/64/80/96
  and Tailwind radii).
- **Query languages have one source.** The `languages` array in `index.astro` gives each
  language a `status` (`available`, `beta` or `planned`) and its version of the stage question.
  The hero list, the stage tabs, the stage caption, the GQL section and the FAQ all read it, so
  moving a language to another status is a one-line change. GQL is available, natural language
  is in beta and Cypher and SQL/PGQ are planned.
- **Natural language is shown as the beta ships it.** The beta's routing model runs on chain, which is
  what the hero's "Native on-chain intelligence" refers to. It matches a question to one of the
  graph's published prepared queries and fills in its typed parameters; it never writes a query
  from free text (`routingSentence`, used by the FAQ). The stage's natural language tab shows
  that route over the five prepared queries in `demo/knowledge/prepared` (`publishedQueries`):
  the question lands on `team-readable-documents` and fills its one parameter, `query`, with
  the topic sent as an embedding (`routedQuery`, `routedParams`).
  The stage opens on this tab (`defaultLanguageId`), because the question is readable without
  knowing any query language and the route shows the trust story in one view.
- **Ownership is stated as it stands.** The page says what is true now: each graph has
  canisters of its own, no rented server and no per query fees. It does not say the graph owner
  controls them; the facts and the FAQ say an operator holds the controller role today and that
  handing it over is on the roadmap. Revisit the tagline and the "Your graph, its own canisters"
  fact when that ships.
- **The stage question is one a real app would get.** "What has the Platform team written about
  access control?" is a reader's question in a team knowledge base, answered by a query the
  knowledge demo publishes and grants (`team-readable-documents`). It puts graph structure,
  vector search and a visibility rule in one plan, and the result row is what the demo's seed
  data returns (the Platform team owns one public document). Pick a question with parameters
  if this changes, so the natural language route still shows a parameter being filled.
- **The stage shows one question in four languages over one plan.** The tabs switch the query
  text only. The plan and the result are rendered once (`stage` in `index.astro`), and the four
  query panels share a grid cell, so the query window keeps the height of the longest query and
  the plan never moves when the language changes.
- **Section order** follows what a reader asks in turn: capabilities, what each caller
  may see (`#permissions`, the three places the caller's grants apply, from
  `design/security/rbac-and-prepared.md`: query admission, every search candidate, and the
  context handed to a model, so AI is one consequence of the access model rather than its
  subject), the tagline, languages, how a query runs, why it is
  built for the canister, what you can build (`#examples`, the knowledge and social demos),
  docs, FAQ and the closing CTA. The header links five stops in the same order: Capabilities,
  Permissions (`#permissions`), Languages, Examples and Docs. "How a query runs" is reached from the
  hero's "See how a query runs" link instead, which keeps the header to five items at 761px.
- **Section layouts alternate.** Capabilities, "How a query runs" and the guides put the heading
  above full-width content; permissions, GQL, the facts, the examples and the FAQ are
  `home-split` (heading in the left 5/12, content in the right 7/12).
- **The plan rail is the recurring form.** A marker and the line to the next one: the plan in
  the stage, the prepared-query figure (same `.plan` classes) and the numbered stages of "How a
  query runs" (`.steps`, horizontal on desktop, vertical below 1180px).
- **Capability figures are HTML drawn from data.** Every figure's type keeps its real size at
  every width, and every figure is computed from values in `index.astro` rather than placed by
  hand:
  - Graph traversal draws `citation-reach` over the demo's seed data
    (`demo/knowledge/seeds/edges.jsonl`): one column per hop, one row per branch, and a dashed
    box for the rest of the `{1,3}` bound where nothing more is cited. It is an SVG whose node
    positions and Bézier edges are computed in `tree` from `traversal`, so every curve ends on
    the dot it joins. Its 1200-unit viewBox renders near 1:1 in the full width tile; at 1180px
    and below, where it would shrink its type, an indented list built from the same data takes
    its place. Only one of the two is displayed at a time.
  - Full text search draws `textBlocks` against `textThreshold`: bar length is the block max
    score, the accent line is the top k threshold.
  - Vector search places example points in a 100 × 60 plane drawn to scale. The probed
    partitions follow the rule in `design/index/vector-index.md` (centroid within
    `(1 + eps)` of the nearest one), candidates are the points of probed partitions, and the
    top k are the nearest candidates the caller may see. The dashed ring is that rule.
  - Authorization lists three requirements of the stage's query beside the grant in
    `demo/knowledge/grants/knowledge.gql` that covers each.
  - Prepared queries shows the three real commands for the stage's query: `gleaph prepared
    apply`, its `GRANT EXECUTE` line and the generated `client.teamReadableDocuments()`.
- **The architecture figure carries the step numbers** of "How a query runs" on the parts that
  run each step (two shards both carry 3).
- **Code is printed in the page's inks.** The `--code-*` tokens alias the slate, clay and ochre
  tokens, so there is no separate syntax palette per theme.
- **One primary action.** "Run the quickstart" appears in the hero and again in the closing
  CTA; everything else is a text link or a documentation link.
- **Plain-language framing.** The hero links the first mention of the Internet Computer to
  `https://internetcomputer.org/`; the FAQ defines the network and what a canister is (a smart
  contract holding code and data), and the facts repeat the on-chain consequence. Other
  mentions stay unlinked. The licence is stated in the closing note, the footer and the FAQ,
  not in the hero.
- **Numbers only where they are a sequence.** Sections carry no 01/02 markers. The only
  numbered content is the three stages of "How a query runs"; the 404 kicker is a status code,
  not a section index.
- **Motion.** Hover and press feedback runs at 150ms (`--t-hover`). Beyond that: the halftone
  sweep, the hero entrance, one reveal for the capabilities section (the first section
  below the hero; every later section is simply there when the reader arrives), and a
  tagline whose words light in reading order as they cross a line at 62% of the viewport.
  The hero halftone also drifts at 0.26× the scroll speed (0.15× below 760px), updated once per
  frame and stopped after one viewport. `prefers-reduced-motion` removes all of it.
- **Progressive enhancement.** Without JavaScript, the four query panels stack and stay
  readable (the stacking rules are scoped to `.js`; the stage bar and its tabs are hidden), the
  tagline is at full opacity, and all content is visible.

Deviations, kept deliberately or because the repository has no material to satisfy the
default: a full-width sticky topbar (not a floating pill), the brand's sand and slate dark
tokens rather than the skill's dark background list, custom stroke icons drawn to match
Geist instead of an icon package, and a solid hero heading instead of the B5 text gradient
(all four follow the CUI brief or an explicit client preference).

There are no privacy or terms pages in this repository, so the footer links the README,
design documents, demo, license and GitHub rather than inventing legal copy. There are no
customers or testimonials to quote, so proof is the licence, the ISO standard and the
clearly labelled example rows; no social proof is fabricated. The graph drawing's labels are
set larger below 760px so they stay readable, while the architecture figure is replaced by a
one-sentence summary at that width. Canonical and social URLs use `https://gleaph.com` from
`astro.config.mjs`; set `PUBLIC_SITE_URL` when the page is served from another host.

## The art

`src/lib/dither.ts` renders `public/art/dunes.jpg` as an 8×8 ordered (Bayer) dither into
a small ink palette, one dot per 2 CSS pixels, with each dot a whole number of device
pixels. The page owns the masks and opacity (`.home-art`, `.home-cta-art`); the module
owns the painting, the light band that sweeps across the picture every 14 seconds, the
reveal, and the theme repaint.

- Light theme prints dark ink where the photograph is dark; dark theme prints pale ink
  where it is light.
- The sweep pauses off-screen and when the document is hidden, repaints at ≤20 fps, and
  with `prefers-reduced-motion: reduce` the art is a single static frame.
- `mountDither(target, options)` returns a cleanup function that removes the wrapper and
  disconnects its observers. The defaults are tuned for the hero; the closing band passes
  `{ focusY: 0.16, gamma: 1.12 }` to crop the mountains instead of the dunes.

The photograph is "Sunset on Dunes and Crestone Peaks", Great Sand Dunes National Park,
by NPS / Patrick Myers. It is a U.S. government work in the public domain; the credit is
rendered below the closing band.

## Notes for the next change

- Theme is resolved before first paint from `localStorage['gleaph-theme']`, falling back
  to `prefers-color-scheme`. The document carries both `data-theme` and a `.dark` class so
  `shared/tokens.css` (which keys on `.dark`) and the art module (which reads `data-theme`)
  agree.
- The stage's GQL text is copied from `demo/knowledge` (`team-readable-documents.gql`), and the
  GQL section's query from `document-text-search.gql`; keep both in sync with those files. The
  Cypher and SQL/PGQ texts are the same query in each language's syntax with the same `SEARCH`
  extension GQL uses, written for the page: nothing executes them yet. The stage's result row
  and the traversal figure follow the demo's seed data; the other figures use example values,
  not benchmark claims.
- The `FAQPage` JSON-LD is generated from the same `faq` array as the visible FAQ, so the
  two cannot drift.
- `astro.config.mjs` points the `cookie` specifier at the copy Astro itself depends on.
  Astro 7 externalises it into the prerender entry, and under pnpm's strict layout the app
  has no `cookie` link, so Node would otherwise resolve an unrelated copy above the
  workspace and fail the build. Revisit when Astro bundles it again.
- Regenerate `public/og.jpg` from a 1200×630 screenshot of the top of the page after a
  visual change. The layout uses `site` for the canonical URL, `og:url` and the absolute
  `og:image`; there is no sitemap integration yet.
