# Data provenance and licences

This demo's dataset is generated, not hand-written. Two licences apply.

## Facts — CC0

Film, person, studio and genre facts (titles, release years, credits, studios, genres, sequel
links) come from **Wikidata** (https://www.wikidata.org), which publishes its data under
**CC0 1.0**. Every `Film`, `Person` and `Studio` row keeps its Wikidata QID in the `qid`
property, so any row can be traced back to its source item.

## Synopsis text — CC BY-SA 4.0

Each `Film.synopsis` is the leading section of the film's Japanese Wikipedia article, retrieved
through the MediaWiki API and stored verbatim (whitespace collapsed). That text is licensed
**CC BY-SA 4.0** (https://creativecommons.org/licenses/by-sa/4.0/). Every row carries the
`sourceUrl` of the article it came from; the demo page links to it. If the dataset is
redistributed, this notice and the per-row `sourceUrl` must travel with it.

No poster, still or other image is included. IMDb and TMDB data are not used.

## Embeddings

`seeds/embeddings.jsonl` is derived from the synopsis text with the pinned multilingual model
recorded in `seeds/embeddings.provenance.json`. Those vectors are a derivative work of the
CC BY-SA text and carry the same licence.
