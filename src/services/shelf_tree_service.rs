//! Invariants of the shelf tree, carried by `tags.parent_id`.
//!
//! A shelf moved under itself or under one of its own sub-shelves forms a
//! cycle: the branch has no root any more and drops out of every tree view.
//! [`ensure_parent_allowed`] is the single gate every shelf update goes
//! through. Cycles can still arrive through sync (two devices moving shelves
//! concurrently), so readers must also tolerate them; this only stops a local
//! write from creating one.

use std::collections::{HashMap, HashSet};

use sea_orm::{ConnectionTrait, EntityTrait};

use crate::models::tag;
use crate::services::book_service::ServiceError;

/// The refusal message, shared by every caller so the wording stays one.
pub const SHELF_CYCLE_ERROR: &str =
    "a shelf cannot be moved under itself or one of its sub-shelves";

/// Refuse a move of shelf `tag_id` under `new_parent` when that parent is the
/// shelf itself or one of its descendants. Moving to the first level (`None`)
/// is always allowed.
pub async fn ensure_parent_allowed<C>(
    conn: &C,
    tag_id: &str,
    new_parent: Option<&str>,
) -> Result<(), ServiceError>
where
    C: ConnectionTrait,
{
    let Some(new_parent) = new_parent else {
        return Ok(());
    };
    if new_parent == tag_id {
        return Err(ServiceError::InvalidInput(SHELF_CYCLE_ERROR.to_owned()));
    }
    let parents: HashMap<String, Option<String>> = tag::Entity::find()
        .all(conn)
        .await?
        .into_iter()
        .map(|t| (t.id, t.parent_id))
        .collect();
    if is_self_or_descendant(tag_id, new_parent, &parents) {
        return Err(ServiceError::InvalidInput(SHELF_CYCLE_ERROR.to_owned()));
    }
    Ok(())
}

/// Whether `candidate` is `tag_id` or sits somewhere below it, found by
/// climbing from `candidate` towards the root. Terminates on a cycle already
/// in the data (merged from sync) that does not go through `tag_id`.
fn is_self_or_descendant(
    tag_id: &str,
    candidate: &str,
    parents: &HashMap<String, Option<String>>,
) -> bool {
    let mut seen = HashSet::new();
    let mut current = Some(candidate);
    while let Some(id) = current {
        if id == tag_id {
            return true;
        }
        if !seen.insert(id) {
            return false;
        }
        current = parents.get(id).and_then(|p| p.as_deref());
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(edges: &[(&str, Option<&str>)]) -> HashMap<String, Option<String>> {
        edges
            .iter()
            .map(|(id, parent)| (id.to_string(), parent.map(str::to_owned)))
            .collect()
    }

    #[test]
    fn a_shelf_is_its_own_descendant() {
        let parents = tree(&[("a", None)]);
        assert!(is_self_or_descendant("a", "a", &parents));
    }

    #[test]
    fn a_grandchild_is_a_descendant() {
        let parents = tree(&[("a", None), ("b", Some("a")), ("c", Some("b"))]);
        assert!(is_self_or_descendant("a", "c", &parents));
    }

    #[test]
    fn a_sibling_or_an_ancestor_is_not() {
        let parents = tree(&[("a", None), ("b", Some("a")), ("c", Some("a"))]);
        assert!(!is_self_or_descendant("b", "c", &parents), "sibling");
        assert!(!is_self_or_descendant("b", "a", &parents), "ancestor");
    }

    #[test]
    fn an_unrelated_cycle_in_the_data_terminates() {
        // x <-> y came from sync; moving `a` under `x` must answer, not spin.
        let parents = tree(&[("a", None), ("x", Some("y")), ("y", Some("x"))]);
        assert!(!is_self_or_descendant("a", "x", &parents));
    }

    #[test]
    fn an_unknown_parent_is_not_a_descendant() {
        let parents = tree(&[("a", None)]);
        assert!(!is_self_or_descendant("a", "ghost", &parents));
    }
}
