//! Style-chain traversal: walk a parent (`basedOn`) chain child-to-root.
//! A cycle is a resource-safety bound and hard-fails - recovery never
//! weakens resource safety.

use crate::ctx::ParseCtx;
use crate::error::ConvertError;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

/// Style id -> (definition, parent style id).
pub struct StyleChains<'a, D> {
    raw: HashMap<&'a str, (&'a D, Option<&'a str>)>,
}

pub type Memo<'a, T> = RefCell<HashMap<&'a str, T>>;

type Link<'a, D> = (&'a str, &'a D, Option<&'a str>);

impl<'a, D> Default for StyleChains<'a, D> {
    fn default() -> Self {
        StyleChains { raw: HashMap::new() }
    }
}

impl<'a, D> StyleChains<'a, D> {
    pub fn insert(&mut self, id: &'a str, def: &'a D, parent: Option<&'a str>) {
        self.raw.insert(id, (def, parent));
    }

    pub fn definition(&self, id: &str) -> Option<&'a D> {
        self.raw.get(id).map(|(d, _)| *d)
    }

    fn link(&self, id: &str) -> Option<Link<'a, D>> {
        self.raw.get_key_value(id).map(|(&id, &(def, parent))| (id, def, parent))
    }

    /// Resolve `id` by folding its chain root-to-child onto `root`, each id
    /// resolved once into `memo` and charged one unit when it is, walking up
    /// to an id already resolved, a definition that `decides` alone or an
    /// unknown id, and failing on a cycle below that stop.
    pub fn resolve<T: Clone>(
        &self,
        parse_ctx: &ParseCtx,
        memo: &Memo<'a, T>,
        id: &str,
        root: T,
        decides: impl Fn(&'a D) -> bool,
        fold: impl Fn(&'a D, T) -> T,
    ) -> Result<T, ConvertError> {
        let mut path: Vec<(&'a str, &'a D)> = Vec::new();
        let mut on_path: HashSet<&str> = HashSet::new();
        let mut resolved = root;
        let mut cursor = self.link(id);
        while let Some((current, def, parent)) = cursor {
            if let Some(hit) = memo.borrow().get(current) {
                resolved = hit.clone();
                break;
            }
            if !on_path.insert(current) {
                return Err(cycle());
            }
            parse_ctx.charge(1)?;
            path.push((current, def));
            if decides(def) {
                break;
            }
            cursor = parent.and_then(|p| self.link(p));
        }
        for (current, def) in path.into_iter().rev() {
            resolved = fold(def, resolved);
            memo.borrow_mut().insert(current, resolved.clone());
        }
        Ok(resolved)
    }

    /// The first value `own` finds for an id along `id`'s chain, child
    /// first, without a memo: each id the walk looks up is charged its length
    /// in bytes (at least one), which is what hashing it costs, found or not,
    /// and a walk longer than the definitions has met a cycle.
    pub fn find<T>(
        &self,
        parse_ctx: &ParseCtx,
        id: &str,
        own: impl Fn(&'a str) -> Option<T>,
    ) -> Result<Option<T>, ConvertError> {
        let mut cursor = self.look_up(parse_ctx, id)?;
        let mut steps = 0;
        while let Some((current, _, parent)) = cursor {
            if steps == self.raw.len() {
                return Err(cycle());
            }
            steps += 1;
            if let Some(found) = own(current) {
                return Ok(Some(found));
            }
            cursor = match parent {
                Some(parent) => self.look_up(parse_ctx, parent)?,
                None => None,
            };
        }
        Ok(None)
    }

    fn look_up(&self, parse_ctx: &ParseCtx, id: &str) -> Result<Option<Link<'a, D>>, ConvertError> {
        parse_ctx.charge(u64::try_from(id.len()).unwrap_or(u64::MAX).max(1))?;
        Ok(self.link(id))
    }
}

fn cycle() -> ConvertError {
    ConvertError::malformed("style inheritance cycle")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::{Limit, Limits};

    #[test]
    fn deep_chain_walks_to_root() {
        let defs: Vec<(String, Option<bool>)> =
            (0..30).map(|i| (format!("s{i}"), if i == 0 { Some(true) } else { None })).collect();
        let mut chains: StyleChains<Option<bool>> = StyleChains::default();
        for (i, (id, d)) in defs.iter().enumerate() {
            let parent = if i == 0 { None } else { Some(defs[i - 1].0.as_str()) };
            chains.insert(id, d, parent);
        }
        let hit = first(&ParseCtx::default(), &chains, &Memo::default(), "s29").unwrap();
        assert_eq!(hit, Some(true));
    }

    #[test]
    fn child_setting_wins_over_parent() {
        let on = Some(true);
        let off = Some(false);
        let mut chains: StyleChains<Option<bool>> = StyleChains::default();
        chains.insert("base", &on, None);
        chains.insert("kid", &off, Some("base"));
        let hit = first(&ParseCtx::default(), &chains, &Memo::default(), "kid").unwrap();
        assert_eq!(hit, Some(false));
    }

    #[test]
    fn cycle_is_hard_error() {
        let none: Option<bool> = None;
        let mut chains: StyleChains<Option<bool>> = StyleChains::default();
        chains.insert("a", &none, Some("b"));
        chains.insert("b", &none, Some("a"));
        assert!(first(&ParseCtx::default(), &chains, &Memo::default(), "a").is_err());
    }

    #[test]
    fn dangling_parent_ends_the_walk() {
        let none = None;
        let mut chains: StyleChains<Option<bool>> = StyleChains::default();
        chains.insert("kid", &none, Some("ghost"));
        assert_eq!(first(&ParseCtx::default(), &chains, &Memo::default(), "kid").unwrap(), None);
    }

    #[test]
    fn folds_every_definition_when_none_decides() {
        let a = Some(true);
        let b = Some(false);
        let mut chains: StyleChains<Option<bool>> = StyleChains::default();
        chains.insert("base", &b, None);
        chains.insert("kid", &a, Some("base"));
        let seen = chains
            .resolve(&ParseCtx::default(), &Memo::default(), "kid", 0u32, |_| false, |_, n| n + 1)
            .unwrap();
        assert_eq!(seen, 2);
    }

    fn chain(defs: &[(String, Option<bool>)]) -> StyleChains<'_, Option<bool>> {
        let mut chains = StyleChains::default();
        for (i, (id, d)) in defs.iter().enumerate() {
            let parent = i.checked_sub(1).map(|p| defs[p].0.as_str());
            chains.insert(id.as_str(), d, parent);
        }
        chains
    }

    fn first<'a>(
        parse_ctx: &ParseCtx,
        chains: &StyleChains<'a, Option<bool>>,
        memo: &Memo<'a, Option<bool>>,
        id: &str,
    ) -> Result<Option<bool>, ConvertError> {
        chains.resolve(parse_ctx, memo, id, None, |d| d.is_some(), |d, inherited| d.or(inherited))
    }

    #[test]
    fn a_resolved_chain_charges_each_definition_once() {
        let defs: Vec<(String, Option<bool>)> =
            (0..4000).map(|i| (format!("s{i}"), (i == 0).then_some(true))).collect();
        let chains = chain(&defs);
        let memo = Memo::default();
        let parse_ctx = ParseCtx::default();
        for (id, _) in &defs {
            assert_eq!(first(&parse_ctx, &chains, &memo, id).unwrap(), Some(true));
        }
        assert_eq!(parse_ctx.fuel(), 4000);
        let limits = Limits { max_work: 3999, ..Limits::default() };
        let err = first(&ParseCtx::new(&limits), &chains, &Memo::default(), "s3999").unwrap_err();
        assert!(matches!(err, ConvertError::ResourceLimit { limit: Limit::Work, max: 3999 }));
    }

    #[test]
    fn an_undecided_chain_charges_each_definition() {
        let defs: Vec<(String, Option<bool>)> = (0..30).map(|i| (format!("s{i}"), None)).collect();
        let parse_ctx = ParseCtx::default();
        assert_eq!(first(&parse_ctx, &chain(&defs), &Memo::default(), "s29").unwrap(), None);
        assert_eq!(parse_ctx.fuel(), 30);
    }

    #[test]
    fn resolving_stops_at_a_deciding_definition_below_a_cycle() {
        let (on, none) = (Some(true), None);
        let mut chains: StyleChains<Option<bool>> = StyleChains::default();
        chains.insert("a", &none, Some("b"));
        chains.insert("b", &none, Some("a"));
        chains.insert("kid", &on, Some("a"));
        let memo = Memo::default();
        let parse_ctx = ParseCtx::default();
        assert_eq!(first(&parse_ctx, &chains, &memo, "kid").unwrap(), Some(true));
        assert!(first(&parse_ctx, &chains, &memo, "a").is_err());
        assert_eq!(memo.borrow().len(), 1);
    }

    #[test]
    fn finding_charges_every_step_and_fails_on_a_cycle_it_walks_into() {
        let defs: Vec<(String, Option<bool>)> = (0..30).map(|i| (format!("s{i}"), None)).collect();
        let chains = chain(&defs);
        let named = |id: &str| (id == "s10").then_some(id.len());
        let parse_ctx = ParseCtx::default();
        for _ in 0..2 {
            assert_eq!(chains.find(&parse_ctx, "s29", named).unwrap(), Some(3));
        }
        assert_eq!(parse_ctx.fuel(), 2 * 20 * 3);
        assert_eq!(chains.find(&parse_ctx, "s9", named).unwrap(), None);
        assert_eq!(chains.find(&parse_ctx, "ghost", named).unwrap(), None);
        let none = None;
        let mut cyclic: StyleChains<Option<bool>> = StyleChains::default();
        cyclic.insert("a", &none, Some("b"));
        cyclic.insert("b", &none, Some("a"));
        cyclic.insert("kid", &none, Some("a"));
        let kid = |id: &str| (id == "kid").then_some(());
        assert_eq!(cyclic.find(&ParseCtx::default(), "kid", kid).unwrap(), Some(()));
        let walked = ParseCtx::default();
        assert!(matches!(cyclic.find(&walked, "a", kid), Err(ConvertError::Malformed { .. })));
        assert_eq!(walked.fuel(), 4);
    }

    #[test]
    fn finding_charges_a_missing_parent_its_length() {
        let none: Option<bool> = None;
        let fuel_at = |width: usize| {
            let missing = "m".repeat(width);
            let mut chains: StyleChains<Option<bool>> = StyleChains::default();
            chains.insert("a", &none, Some(missing.as_str()));
            let parse_ctx = ParseCtx::default();
            assert_eq!(chains.find(&parse_ctx, "a", |_| None::<()>).unwrap(), None);
            parse_ctx.fuel()
        };
        assert_eq!((fuel_at(1), fuel_at(4096)), (1 + 1, 1 + 4096));
        let parse_ctx = ParseCtx::default();
        let empty: StyleChains<Option<bool>> = StyleChains::default();
        assert_eq!(empty.find(&parse_ctx, &"g".repeat(4096), |_| None::<()>).unwrap(), None);
        assert_eq!(parse_ctx.fuel(), 4096);
    }

    #[test]
    fn finding_charges_each_step_by_the_length_of_its_id() {
        let depth: u64 = 30;
        let fuel_at = |width: usize| {
            let defs: Vec<(String, Option<bool>)> =
                (0..depth).map(|i| (format!("{i:0>width$}"), None)).collect();
            let chains = chain(&defs);
            let parse_ctx = ParseCtx::default();
            assert_eq!(chains.find(&parse_ctx, &defs[29].0, |_| None::<()>).unwrap(), None);
            parse_ctx.fuel()
        };
        assert_eq!(fuel_at(8), depth * 8);
        assert_eq!(fuel_at(4096), depth * 4096);
        let defs: Vec<(String, Option<bool>)> =
            (0..depth).map(|i| (format!("{i:0>4096}"), None)).collect();
        let limits = Limits { max_work: depth * 4096 - 1, ..Limits::default() };
        let refused = chain(&defs).find(&ParseCtx::new(&limits), &defs[29].0, |_| None::<()>);
        assert!(matches!(refused, Err(ConvertError::ResourceLimit { limit: Limit::Work, .. })));
    }

    #[test]
    fn finding_charges_an_empty_id_one_unit() {
        let none: Option<bool> = None;
        let mut chains: StyleChains<Option<bool>> = StyleChains::default();
        chains.insert("", &none, None);
        chains.insert("kid", &none, Some(""));
        let parse_ctx = ParseCtx::default();
        assert_eq!(chains.find(&parse_ctx, "kid", |_| None::<()>).unwrap(), None);
        assert_eq!(parse_ctx.fuel(), 3 + 1);
    }

    #[test]
    fn a_fold_inherits_through_a_resolved_parent() {
        let defs: Vec<(String, Option<bool>)> =
            (0..3).map(|i| (format!("s{i}"), Some(i != 1))).collect();
        let chains = chain(&defs);
        let memo = Memo::default();
        let parse_ctx = ParseCtx::default();
        let parity = |id| {
            chains.resolve(
                &parse_ctx,
                &memo,
                id,
                0u32,
                |_| false,
                |d, n| n + u32::from(*d == Some(true)),
            )
        };
        assert_eq!(parity("s1").unwrap(), 1);
        assert_eq!(parity("s2").unwrap(), 2);
        assert_eq!(parity("ghost").unwrap(), 0);
        assert_eq!(parse_ctx.fuel(), 3);
    }
}
