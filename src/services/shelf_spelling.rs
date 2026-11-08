//! Which shelf a name stored in a book's `subjects` stands for.
//!
//! A book files its shelves by name, and a name may differ from its `tags`
//! row by case alone ("polar" under a shelf "Polar"): typed before names were
//! checked, imported, or merged from sync. Opening a shelf matches its books
//! whatever the case, so everything that counts, removes or renames a shelf in
//! the books goes by the same rule, or a shelf announces books it does not
//! hold, or a delete leaves some of them behind.
//!
//! The rule: a name some row carries exactly is that row's; otherwise the
//! first row, in name order, that differs from it by case alone; otherwise it
//! stands for itself. Accents stay apart.

use std::collections::{HashMap, HashSet};

use sea_orm::{ConnectionTrait, EntityTrait, QueryOrder};

use crate::models::tag;

/// The shelves of a library, by the spellings books may file them under.
#[derive(Debug, Default)]
pub struct ShelfSpelling {
    exact: HashSet<String>,
    by_case: HashMap<String, String>,
}

impl ShelfSpelling {
    /// From the names the `tags` rows carry, in any order.
    pub fn new<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut names: Vec<String> = names.into_iter().map(Into::into).collect();
        // Name order, as the listing reads the rows: the first row wins.
        names.sort();
        let mut by_case: HashMap<String, String> = HashMap::new();
        for name in &names {
            by_case
                .entry(name.trim().to_lowercase())
                .or_insert_with(|| name.clone());
        }
        Self {
            exact: names.into_iter().collect(),
            by_case,
        }
    }

    /// The names of the `tags` rows, read from `conn`, to build one from.
    pub async fn row_names<C: ConnectionTrait>(conn: &C) -> Result<Vec<String>, sea_orm::DbErr> {
        Ok(tag::Entity::find()
            .order_by_asc(tag::Column::Name)
            .all(conn)
            .await?
            .into_iter()
            .map(|t| t.name)
            .collect())
    }

    /// The shelf `name` stands for, spelled as its row is.
    pub fn shelf_of<'a>(&'a self, name: &'a str) -> &'a str {
        let name = name.trim();
        if self.exact.contains(name) {
            return name;
        }
        self.by_case
            .get(&name.to_lowercase())
            .map_or(name, String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::ShelfSpelling;

    #[test]
    fn a_name_differing_by_case_alone_stands_for_its_shelf() {
        let spelling = ShelfSpelling::new(["Polar", "Étrangers"]);
        assert_eq!(spelling.shelf_of("polar"), "Polar");
        assert_eq!(spelling.shelf_of(" POLAR "), "Polar");
        assert_eq!(spelling.shelf_of("étrangers"), "Étrangers");
    }

    #[test]
    fn accents_stay_apart_and_an_unknown_name_stands_for_itself() {
        let spelling = ShelfSpelling::new(["Étagère"]);
        assert_eq!(spelling.shelf_of("etagere"), "etagere");
        assert_eq!(spelling.shelf_of("Roman"), "Roman");
    }

    #[test]
    fn an_exact_row_keeps_its_name_and_the_first_row_takes_the_others() {
        let spelling = ShelfSpelling::new(["Polar", "POLAR"]);
        assert_eq!(spelling.shelf_of("Polar"), "Polar");
        assert_eq!(spelling.shelf_of("POLAR"), "POLAR");
        // "POLAR" sorts before "Polar".
        assert_eq!(spelling.shelf_of("polar"), "POLAR");
    }
}
