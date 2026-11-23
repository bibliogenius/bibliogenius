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
  to each. robots.txt paths and patterns are compared normalized (RFC 9309):
  `Disallow: /recherche-avancée` matches `/recherche-avanc%C3%A9e`.
- A host that turned us away once (429, a firewall's rejection page) is
  not asked again in the same run: asking again over many books is what gets
  an address banned.
- One lookup lasts 60 s at most in the app (`LOOKUP_DEADLINE`); the sites not
  done with by then are `unreachable`. The app also leaves alone a site whose
  Crawl-delay is longer than it can wait behind a click. `measure` gives each
  book 15 minutes: its books of one host wait for each other's turn there
  (Gallimard asks 10 s between two requests), and the app's deadline would
  measure that queue rather than the recipes. `measure --like-the-app` looks
  for one book at a time with the app's pause, Crawl-delay cap and deadline,
  to see what a click costs.

## Refreshing the table (before a release)

```sh
cargo build --release --bin publisher_sites
./target/release/publisher_sites wikidata        # publishers and sites, 978-2 / 979-10
./target/release/publisher_sites check           # replay the table's recipes (minutes)
./target/release/publisher_sites probe --new     # sites never probed (resumable)
./target/release/publisher_sites build           # -> table.json
./target/release/publisher_sites lint            # recipes a probe got wrong (offline)
./target/release/publisher_sites diff            # what changed since HEAD (--base REF|FILE)
./target/release/publisher_sites measure --books FILE  # covers found for a list of books
./target/release/publisher_sites report          # -> target/publisher_sites/index.html
```

`lint` reads the table and `probe.json` only, and fails on an error: a
product template with one book's number frozen in it (`/2043-{slug}-{isbn}.html`,
five digits or more, or three glued to a placeholder), a witness titled
after the site (its page's title was not read), or a recipe that sends a
token issued for one visit (`_wpnonce`, a CSRF field, a long hex value),
which breaks once the token expires: the probe leaves such fields out of the
recipes it writes, and `build` does not ship an older probe's recipe that
has one. Its warnings are templates
with a fixed section of three words or more, which serve that section only,
site maps cut at the 2 MB page cap when probed (the books listed after the
cut are out of reach), and sites without a conclusive success for 120 days. Fix an error with a
correction in `overrides.json`, or probe the site again. `diff` lists the
sites added and removed, the recipes and the prefixes that changed: read it
before committing a new table rather than the JSON diff.

`probe` options: `--site a.fr,b.fr` (only these), `--status blocked` (only
sites whose last probe ended so), `--limit N`, `--workers N` (sites probed at
once, default 12), `--pause SECS` (between two requests to one host, default
1.5, more when the site's Crawl-delay asks). `probe.json` is saved after every
site, so a run can be stopped and resumed with `--new`. It is written one site
per line, in key order, so its history shows which sites a probe or a check
changed; a test keeps the committed file in that form. A whole run takes a
few hours: about 100 requests per site. A probe that finds a site blocked or
offline keeps the site's last successful probe: a bad day says nothing about
its recipes, and the checks take a site that stays out of reach out of the
table.

## How the table is checked

`check` replays each recipe of `table.json` on the site's witnesses, the way
the app will use it: a few requests per site instead of a hundred. A site
map recipe is replayed too: the map is read, and the pages it gives for a
witness opened.

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

1. Witnesses: books of the site, read on their own page (ISBN, title,
   author), plus the `witnesses` of `overrides.json`. They are looked for,
   until three are found, behind the links whose path announces a book (an
   ISBN, or a word such as `livre`, `ouvrage`, `catalogue`, `product`,
   `accueil`, `roman`, `essai`): on the home page, then on the catalogue page
   it links, then among the pages of the site map (the one robots.txt
   declares, else `/sitemap.xml`). When no path announces a book, a few links
   are opened at three places of each of these lists: one that shows a single
   ISBN is a book, and its neighbours are opened next.
   Two older books are then taken further down the site map, among the pages
   of the same kind: the home page shows this month's books, and a recipe
   must hold for the backlist too. The site map that lists the witnesses is
   kept in `probe.json` (`book_sitemap`).
2. Product page: the witnesses' addresses with `{isbn}`, `{slug}` and
   `{author}` put back; kept when it rebuilds at least two of them, or when the
   server finds the book by its ISBN whatever the slug.
   An address without the ISBN is a pattern too when the title's slug is in
   it (`/livre/{slug}/`): the page is named by its title alone, and the ISBN it
   shows tells whether it is the right book.
3. Search: the search forms of the home page, `/catalogue`, `/recherche`,
   `/search` (a POST form is submitted once and the address it redirects to
   kept), then usual addresses, WordPress's `/?s=` first. A nonsense query is run first: a page that
   already links the witnesses proves nothing, and an answer that never
   changes means results drawn by JavaScript.
4. Site map: when the map that lists the witnesses leads back to at least
   two of them, and two thirds, from their ISBN or title alone, it is a
   recipe too (`sitemap` in `probe.json` and `table.json`). The book's page
   is the address carrying the ISBN, else the one named after the title (four
   fifths of its words) with the fewest other words; the two best are opened
   and the ISBN on the page decides. It serves the sites that file each book
   under a section (`/boutique/livres/<section>/<slug>/`) and forbid their
   search in robots.txt. It costs the download of the map (up to 2 MB, to be
   kept for the next books of the site), so it is the recipe to try last. A
   map split in several files is only known by the file listing the
   witnesses.

   To switch site map recipes off, set `SITEMAP_RECIPES` to `false` in
   `src/modules/integrations/publisher_sites/table.rs` and run `build`: the
   probe looks for none and the table ships none.

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

A publisher Wikidata has no ISBN prefix for goes in the `publishers` list of
`overrides.json` (name and home page, by hand: the report page does not edit
it). Its site is probed like any other, and `build` gives it the publisher
prefixes of the books the probe found there, read from the ISBN ranges of the
French-language groups (978-2 and 979-10; a book of another group gives no
prefix). On a prefix that already has publishers, it is added after them.
The better fix is to complete Wikidata, then drop the entry.

A hand-written product template can also use `{isbn_hyphens}`, the ISBN with
its hyphens (`978-2-8180-5511-3`), for a site that wants it so (P.O.L). The
probe never finds such a template itself, and it only works for the books of
the French-language groups.

When several publishers share a prefix, `sites_for` gives their sites, the
one whose name matches the book's publisher field first, three at most, for
the caller to try in turn.

## Measuring the table

```sh
./target/release/publisher_sites measure --books FILE   # -> target/publisher_sites/measure/
./target/release/publisher_sites measure --books FILE --like-the-app   # as a click would
```

`FILE` is a JSON list of books (`isbn`, `title`, `authors`, `publisher`, and
`cover_url`, the cover the library has). Each book is looked for as the app
will (`publisher_sites::lookup`, then `publisher_sites::cover`): the sites of
its ISBN prefix, each recipe in turn, a page taken only when it shows the
ISBN as its book's, then the cover image, loaded through the same polite
client and checked as the app checks it.

By default twelve books are looked for at a time, 15 minutes each at most,
which measures the recipes. `--like-the-app` measures the time instead: one
book at a time, the app's 1 s pause, its 10 s Crawl-delay cap (a site asking
more is left alone) and its 60 s deadline (`LOOKUP_DEADLINE`). Each book's
`seconds` in `results.json` is how long its lookup and cover took. On the 49
reference books whose title is cut (November 2026), 45 took under 16 s; the
slowest were Gallimard's, 43 to 54 s, its 10 s Crawl-delay times four product
tries.

A page shows the ISBN as its book's when its main JSON-LD item (a `Book` or
`Product` at the top of a script, in its `@graph`, or as a page's
`mainEntity`, read from its ISBN fields only) or its ISBN metadata give it,
among six ISBNs at most; a page whose main item gives other ISBNs only is
another edition's, whatever its text says. An `ItemList`, a work's
`workExample` editions, or an address that holds the ISBN prove nothing.
Without such metadata, the visible text must print it, and never as a
search's echo: search words ("résultat", "cherché", "aucun", "nothing
found", "0 produit", "introuvable"...) within 40 bytes before or after any
of its occurrences reject the page, whatever label it has. The page must
also print six ISBNs at most, read as numbers of their own whether spaced
out, hyphenated or ISBN-10 (a list of years loses its spaces into a fake
ISBN otherwise, and an ISBN-10 needs its check digit). A series page prints every
volume's; a book's page prints its other editions too (Actes Sud four,
Minuit three), and some print it with no label at all (Le Tripode, Picquier):
an earlier rule asking for the label, or for three ISBNs at most, lost 25 of
137 covers on the reference measure. The contact sheet (`index.html`, served like
the report) shows each cover found beside the library's: a wrong cover is the
failure that counts, and only an eye finds it. The results stay in `target/`:
they hold the titles of someone's library.

How the cover is chosen on the book's page, each rule from a real case:
- an image named after the book's ISBN first; one named after another ISBN
  is a related title's, never taken (Actes Sud declared one);
- then the first image the page declares (JSON-LD, `og:image`), then the
  `<img>` tags marked as the book's by their class or a close container's
  (`couv_livre`, `image-livre`), for a declared image that is gone (Arléa),
  then the other declared images (often a share card);
- the other `<img>` tags only on a page that declares nothing of its own;
- the first image robots.txt forbids (or its server refuses) ends the
  search: the next images of the page are a close button or a banner (the
  Points and Seuil covers are on `ref.lamartinieregroupe.com`, `Disallow: /`);
- the site's own images are left out: kept in its theme or icon folders, or
  named as a stand-in (`nocover.jpg`, `couverture-manquante.png`); a page
  that declares only a stand-in has no cover.

A product recipe tries four addresses at most, built from the forms of the
title (`text::title_variants`): the whole title, then, when a colon or a
spaced dash cuts it, its two parts swapped and each part alone. Which part
is the book's own title cannot be told from the text: "Donner à manger :
Politique d'un geste ordinaire" is found by its first part, "Une enquête du
commissaire Brunetti : L'Épreuve du feu" by its second. Before changing
these forms or their order, replay them offline on the reference books: a
rule that guessed the title from a volume number pushed the book's own
title out of the tries for eleven of them (series without a number), and
no request was needed to see it.

A search by title is sent the whole title without its colons
(`recipes::search_terms`): some engines read `word:value` as a field
filter. Gallmeister's (Shopify) finds nothing for "Le juge et son
bourreau : le soupçon", and the book for "Le juge et son bourreau le
soupçon" or its first part alone. Its result is an empty link laid over
the card, named by its `aria-label` only, which `page::result_links` reads
along with the link's path, text and `title`.

A book is `not_found` only when its sites answered. A site that did not
answer at all (its home page neither) makes it `unreachable`, and one that
turned us away (403, 429, a firewall's "Request Rejected" page, robots.txt
included) `refused`: both say nothing of the book, so measure them again
rather than count them. Run one full measure a day at most: after three
passes over the same 600 books in a day, the F5 firewalls of Gallimard,
J'ai lu and Flammarion rejected the whole address, the owner's own browser
included, for hours. The app asks for one book at a time and does not meet
this; a bulk pass does.

Reference measure, 2026-11-14: the 226 books of a library, ten at most per
publisher, **137 covers found, none wrong**. The proof of concept found 103
with the same rule (robots.txt applies to images) and a browser User-Agent.
Of the rest: 61 not found on the sites (old ISBNs, mostly), 13 covers
forbidden by robots.txt, 12 books without a site in the table, 3 pages
without a usable image.

The whole library, 2026-11-15: 596 books, no cap per publisher, **326
covers found, none wrong**. 94 of them are for books that neither Inventaire
nor Open Library has a cover for. Of the rest: 159 not found on the sites,
59 books without a site in the table, 21 covers forbidden by robots.txt,
14 refused (J'ai lu and Flammarion rate-limiting a third pass), 10 invalid
ISBNs, 7 pages without a usable image. A cover that looks like another
edition was, each time, the right cover for a wrong ISBN in the library.

## What the first full probe taught (November 2026)

- A site reported `offline` is rarely a passing failure: probed again a day
  later, 104 of 112 were still unreachable. The usual causes are a domain
  the publisher gave up, or a certificate that is expired or does not cover
  the host (Dupuis, Viviane Hamy, Philippe Rey, Milady). Look for the
  publisher's current address and set it as the site's `home` correction.
- Try the other spelling of the host before giving up: `glenat.com` serves an
  expired certificate, `www.glenat.com` works.
- Wikidata often gives a group's corporate site instead of the catalogue
  (`hachette.com`, `editis.com`, `milanpresse.com`, `cnrs.fr`): no witness is
  found there. Same fix, a `home` correction.
- Before adding a publisher to `publishers`, look for it in `wikidata.json`
  under another name: Ouest-France is there as "Édilarge", Quae as
  "Éditions Quæ".
- The ISBN ranges in `build.rs` were written from the agency's published
  ranges and compared with Wikidata: 1858 prefixes agree, 7 differ, all of
  them typing mistakes in Wikidata. They were not compared with the agency's
  range file itself.
- A recipe with a fixed section in its template (`/glenat-bd/{slug}-{isbn}/`)
  only serves that section's books.
- About ten sites split their site map in several files; the site map recipe
  knows one file only.
