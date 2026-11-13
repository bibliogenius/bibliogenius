//! Publisher sites: where to find a book's cover on its publisher's own site.
//!
//! The ISBN prefix names the publisher (Wikidata P3035), the publisher names
//! its site (P856), and each site has a "recipe" to reach a book's page from
//! its ISBN: a product URL pattern, or the site's own search. The recipes are
//! found and checked by a maintenance tool (`tools/publisher_sites`), not by
//! the app: the app reads the table the tool writes (`data/publisher_sites/`),
//! never Wikidata, and corrections made by hand to that table survive every
//! refresh.
//!
//! Rules every fetch follows: the client says who it is (no browser
//! User-Agent), robots.txt is honoured for pages and images alike, a site
//! that turns scripts away is left alone, and a publisher that asks is
//! removed from the table.

pub mod build;
pub mod check;
pub mod fetch;
pub mod page;
pub mod probe;
pub mod robots;
pub mod table;
pub mod text;

#[cfg(test)]
mod test_site;

pub use table::{PublisherTable, SiteRecipes};
