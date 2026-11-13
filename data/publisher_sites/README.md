# Publisher site table

Where the app looks for a book's cover on its publisher's own site: the ISBN
prefix gives the publisher, the publisher gives its site, and each site has a
recipe that leads from an ISBN to the book's page. Code:
`src/modules/integrations/publisher_sites/` (shared with the app) and
`tools/publisher_sites/` (this maintenance tool).

| File | Written by | Ships with the app |
|------|------------|--------------------|
| `wikidata.json` | `publisher_sites wikidata` | no |
| `probe.json` | `publisher_sites probe` | no |
| `overrides.json` | people (by hand or from the report page) | no |
| `table.json` | `publisher_sites build` | yes |

## Rules

- The app never queries Wikidata: it reads `table.json` only.
- The tool and the app say who they are (`BiblioGenius/1.0 (+https://bibliogenius.org)`),
  never pretend to be a browser, honour robots.txt (a `User-agent: BiblioGenius`
  group takes precedence over `*`; a server error on robots.txt forbids the whole
  site) and its `Crawl-delay`, and leave alone a site that turns scripts away.
- A publisher that asks to be left out gets `"excluded": true` in `overrides.json`.
- Only public https addresses are fetched; an http link is tried over https.
  Redirects are followed one hop at a time, robots.txt and the pause applying
  to each.

## Refreshing the table (before a release)

```sh
cargo build --release --bin publisher_sites
./target/release/publisher_sites wikidata        # publishers and sites, 978-2 / 979-10
./target/release/publisher_sites check           # replay the table's recipes (minutes)
./target/release/publisher_sites probe --new     # sites never probed (resumable)
./target/release/publisher_sites build           # -> table.json
./target/release/publisher_sites report          # -> target/publisher_sites/index.html
```

`probe` options: `--site a.fr,b.fr` (only these), `--status blocked` (only
sites whose last probe ended so), `--limit N`, `--workers N` (sites probed at
once, default 12), `--pause SECS` (between two requests to one host, default
1.5, more when the site's Crawl-delay asks). `probe.json` is saved after every
site, so a run can be stopped and resumed with `--new`. A whole run takes a
few hours: about 100 requests per site.

## How the table is checked

`check` replays each recipe of `table.json` on the site's witnesses, the way
the app will use it: a few requests per site instead of a hundred.

- A witness counts only while it is still in the catalogue (its page still
  shows its ISBN). When none is left, new ones are taken from the home page.
- A search is first run on a nonsense query: a witness its answer already
  links (a "not found" page listing the latest books) proves nothing.
- Witnesses given by hand are tried first; each recipe is judged on up to 3.
- A recipe found by the probe that fails: the site is probed again at once.
  If that probe is blocked or offline on the way, the previous one stands.
- A recipe written by hand that fails: left as it is in `overrides.json` and
  marked `broken` ("Last check not passed" filter of the report page).

A hand-written recipe enters the table only once a check has found it works
in its current form: `build` checks by itself those that are new or changed
since their last check (a few seconds per site), and leaves out, with a
warning, those that fail or could not be checked; the probe's recipe for the
site, if any, ships meanwhile. `build --offline` checks nothing and leaves the
unchecked ones out.
- A site blocked or offline keeps its recipes; blocked or offline on 3
  different days in a row, it leaves the table at the next `build`, and
  comes back when it answers again. A server error on robots.txt counts as
  offline, a 429 on it as blocked.

The outcome of each check is kept in `probe.json` (`checks`). `probe
--older-than DAYS` and `probe --status S` still re-probe sites from scratch.

## How a site is probed

1. Witnesses: books linked from the home page, read on their own page (ISBN,
   title, author), plus the `witnesses` of `overrides.json`.
2. Product page: the witnesses' addresses with `{isbn}`, `{slug}` and
   `{author}` put back; kept when it rebuilds at least two of them, or when the
   server finds the book by its ISBN whatever the slug.
3. Search: the search forms of the home page, `/catalogue`, `/recherche`,
   `/search` (a POST form is submitted once and the address it redirects to
   kept), then usual addresses. A nonsense query is run first: a page that
   already links the witnesses proves nothing, and an answer that never
   changes means results drawn by JavaScript.

## Improving the table

Open the report page (`publisher_sites report`, then serve the folder with
`python3 -m http.server`: Chrome refuses some features on `file://`). It
speaks English or French, following the browser's language, with a switch in
its header. Filter by status, try the recipes
on the witnesses, and correct a site: home page, product template, search
address, extra witnesses, exclusion; or send an ISBN prefix to another
publisher. Corrections stay in the browser until exported: replace
`overrides.json` with the exported file, run `build` (it checks the new or
changed recipes itself; run `probe --site KEY` first when the site must be
probed from scratch, for a new home page or new witnesses), and open a PR with
the three files.

Corrections win over Wikidata and over the probe. Common ones: a prefix shared
by several houses (978-2-07: Gallimard, Gallimard Jeunesse, Hoëbeke; the
default is the shortest name, which may be wrong), a wrong site in Wikidata
(Mercure de France points at Gallica), a search the probe could not find
(results loaded by JavaScript from another address: look in the browser's
network tab).
