# Question sources for the held-out graph-type routing gate (2026-11-16)

Question: can the questions for ADR 0124's held-out graph-type gate come from public sources
instead of being written by the project, without concentrating on one kind of asker?

Status: investigation record (append-only), 2026-11-16. No data was downloaded, no gate file was
written and nothing was evaluated. Licence statements were read on the pages listed under Sources on
2026-11-16; they are a reading of public pages, not legal advice.

Markers: `[verified]` = read on the primary page; `[secondary]` = reported by news or forum
coverage, primary page not reachable from this environment; `[repo]` = read in this repository or
in Glev; `[proposal]` = not built.

## Summary

1. Yes, with one change of order: **choose domains where question corpora exist, then write the
   graph type and catalog for each domain.** Found questions are not addressed to a catalog, so the
   catalog has to be fitted to the domain. The movie gate already worked this way with MovieLens
   `[repo]`.
2. Eight usable corpora were confirmed with licences that allow redistribution. Three candidates are
   excluded or on hold: ORCAS (non-commercial research only), the Yahoo! Chiebukuro dataset (new
   provision has ended), and Reddit (terms not confirmed from the primary page).
3. The asker mix can be widened but not balanced. Only two sources are organic users of a real
   service (Natural Questions, MovieLens); the rest are annotators or paid crowd workers. **No
   usable source of organic Japanese questions was found.**
4. Bias is handled by recording strata per row and reporting per stratum, not by claiming a
   representative sample: there is no production traffic to be representative of.

## 1. Sources checked

| Source | Licence | Who wrote the questions | Size and domains | Use here |
| --- | --- | --- | --- | --- |
| Spider 1.0 | CC BY-SA 4.0 `[verified]` | eleven Yale students, against a database schema `[verified]` | 10,181 questions, 200 databases, 138 domains; train and test databases disjoint `[verified]` | yes |
| BIRD | CC BY-SA 4.0 since 2024-04-27; authors state research and "healthy applications" `[verified]` | not stated on the page | 12,751 pairs, 95 databases, 37+ professional domains `[verified]` | yes |
| Natural Questions | Apache-2.0 `[verified]` | real users of Google search `[verified]` | 307,372 train / 7,830 dev; open domain | yes, filtered by domain |
| TyDi QA | Apache-2.0 on the repository `[verified]`; the page does not say it covers the data | people who wanted the answer and had not seen it, written natively per language `[verified]` | 204K across 11 languages including Japanese; Japanese count not on the page | yes, Japanese, filtered |
| MIRACL | Apache-2.0 `[verified]` | native speakers of each language `[verified]` | Japanese: 3,477 train, 860 dev `[verified]` | yes, Japanese, filtered |
| MultiWOZ | MIT `[verified]` | crowd workers on the visitor side of a Wizard-of-Oz task `[verified]` | about 10k dialogues; attraction, hotel, restaurant, taxi, train | yes, first user turns |
| Schema-Guided Dialogue | CC BY-SA 4.0 `[verified]` | dialogue simulator plus paid crowd workers `[verified]` | 20k+ dialogues, 20 domains | yes, first user turns |
| MASSIVE | CC BY 4.0 `[verified]` | crowd workers localizing English SLURP utterances `[verified]` | 52 languages including ja-JP; 18 scenarios, 60 intents | yes, command style, EN and JA |
| CypherBench | Apache-2.0 `[verified]` | templates instantiated on subgraphs, rewritten by an LLM `[verified]` | 11 property graphs from Wikidata; about 10.9k pairs | **schemas only**; its questions are generator-shaped |
| MovieLens query set | CC BY 3.0 US `[repo]` | users of movielens.org | 448 unique queries | already used; the movie catalog is in training |
| ORCAS | "non-commercial research purposes only" `[verified]` | search users | 10.4M queries | **excluded** |
| Yahoo! Chiebukuro (NII IDR) | new provision has ended `[verified]` | users of a Q&A service | — | **unavailable** |
| Reddit | Data API terms reported to forbid training a model on user content without the rightsholders' permission `[secondary]` | forum users | — | **excluded** until the primary terms are read |
| Stack Exchange | contributions are CC BY-SA; the 2024 data-dump agreement is reported to add a no-LLM-training condition, which critics say conflicts with the licence `[secondary]` | forum users | — | **on hold** until the primary terms are read |

`redditinc.com` and `stackoverflow.com` could not be fetched from this environment, which is why
those two rows rest on secondary reports.

## 2. Proposed assembly

Six graph types of about 50 questions each (240 in scope, 60 out of scope), split by graph type into
two dev types and four frozen holdout types, as in the earlier design `[proposal]`.

| Graph type | Derived from | Question source | Asker and style | Language |
| --- | --- | --- | --- | --- |
| G1 relational domain A | a Spider database with four or more tables and a self-reference | Spider questions on that database | annotator, full analytic question | EN |
| G2 relational domain B | a BIRD database in a professional domain (finance or healthcare) | BIRD questions on that database | annotator, full question with domain terms | EN |
| G3 booking | MultiWOZ and SGD service schemas (hotel, restaurant, train) | first user turns | crowd role-play, conversational | EN |
| G4 encyclopedic | one CypherBench graph schema | Natural Questions, TyDi QA and MIRACL filtered to that domain | search user (EN), native information seeker (JA); short factoid | EN + JA |
| G5 assistant | a calendar or music-library graph matching MASSIVE scenarios | MASSIVE en-US and ja-JP | crowd worker, command | EN + JA |
| G6 social | `demo/social` (six prepared queries, no routing evaluation, not in training) `[repo]` | none found | — | gap |

Notes on the table:

- G1 and G2 derive the graph type mechanically (table to label, foreign key to edge), which gives
  topologies outside the item/connector/team template without anyone designing them to be hard.
- Spider and BIRD questions are one-off analytic queries: Spider has 5,693 distinct SQL queries for
  10,181 questions `[verified]`, so a database's questions spread over many different query shapes.
  A prepared catalog is a finite set of registered operations, so a catalog of 6 to 20 operations
  answers only the questions whose shape someone registered; the rest are "no operation covers
  this". That gives an out-of-scope distribution nobody authored. It is a property of catalog
  size, not of aggregation: a prepared query can aggregate, and whether a given `COUNT` question is
  in scope depends on the catalog written for the gate. The risk is the reverse one — too few
  in-scope questions per graph type — and has to be checked per database before it is chosen.
- G4 is the only type that puts English and Japanese questions against the same catalog.
- G6 has no external source. Either it is authored and labelled as such, or it is dropped and the
  gate has five types.

### Strata recorded on every row

`source`, `language`, `style` (keyword, question, conversational, command), `length` bucket, and
`authoring` (organic user, annotator, crowd role-play, localized). Rules `[proposal]`:

- no single source above 30% of the gate;
- a minimum count per `style` and per `language`;
- results reported per stratum beside the total, with the organic-user slice called out.

## 3. Constraints

1. **Labels are still ours.** A source supplies the question, not which operation answers it. Each
   row needs an operation or out-of-scope label against our catalog, with a second annotator on at
   least 30% (the movie record's requirement).
2. **Evaluation only.** None of these rows may enter training. That is the integrity rule for a
   holdout, and it also keeps the gate clear of any no-training condition attached to a source.
3. **Licence mixing.** Spider, BIRD and SGD are CC BY-SA 4.0 and this repository is MIT/Apache.
   Either the gate directory carries its own licence notice and attribution, or the repository
   stores only source ids, labels and a SHA-256 of the assembled file, and a collector script
   fetches the text at a pinned dataset revision into `target/`, as
   `demo/movies/scripts/collect-real-queries.py` does for its download `[repo]`.
4. **Japanese is thin.** MIRACL and TyDi QA are natively written but encyclopedic; MASSIVE ja-JP is
   localized from English commands. None is a Japanese user asking a product a question. Authored
   Japanese items remain necessary and must be labelled as authored.
5. **Not representative.** The mix is wider than one developer's phrasing and narrower than users:
   four of the eight usable sources are paid annotators or crowd workers.

## 4. Open decisions

- Store question text under a separate licence notice, or store ids and fetch at a pinned revision?
- Keep G6 with authored questions, or run five types?
- Who is the second annotator?
- Where the gate lives: existing evaluation files are all under `demo/*/eval/`, and these graph
  types are not demos.

## Sources

Primary pages read 2026-11-16:

- Spider: https://yale-lily.github.io/spider and https://huggingface.co/datasets/xlangai/spider
- BIRD: https://bird-bench.github.io/
- Natural Questions: https://github.com/google-research-datasets/natural-questions
- TyDi QA: https://github.com/google-research-datasets/tydiqa
- MIRACL: https://huggingface.co/datasets/miracl/miracl
- MultiWOZ: https://github.com/budzianowski/multiwoz
- Schema-Guided Dialogue: https://github.com/google-research-datasets/dstc8-schema-guided-dialogue
- MASSIVE: https://huggingface.co/datasets/AmazonScience/massive and https://github.com/alexa/massive
- CypherBench: https://github.com/megagonlabs/cypherbench and https://huggingface.co/datasets/megagonlabs/cypherbench
- ORCAS: https://microsoft.github.io/msmarco/ORCAS.html
- Yahoo! Chiebukuro (NII IDR): https://www.nii.ac.jp/dsc/idr/yahoo/chiebkr3/Y_chiebukuro.html

Secondary reports (primary pages not reachable):

- Reddit Data API terms: https://the-decoder.com/reddit-ends-its-role-as-a-free-ai-training-data-goldmine/ ,
  https://www.theregister.com/2023/04/18/reddit_charging_ai_api/
- Stack Exchange data-dump agreement:
  https://www.devclass.com/development/2024/07/30/stack-exchange-restricts-access-to-dump-of-user-contributed-data-critics-complain-this-contradicts-license/1625192 ,
  https://heise.de/-9823083

Repository: `demo/movies/scripts/collect-real-queries.py`,
`design/investigations/2026-11-12-movie-real-query-gate.md`, `demo/social/prepared/`,
`glev: experiments/general_routing/generate.py`.
