//! Pure member -> slot mapping: which members get a slot, in what order,
//! and which ones only appear in the overflow badge.

use std::cmp::Reverse;
use std::collections::HashMap;

use crate::config::SortOrder;
use crate::state::MemberState;

/// Indices into the member list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Assignment {
    /// One member per slot, slot 0 first.
    pub shown: Vec<usize>,
    /// Members that did not fit (overflow badge).
    pub hidden: Vec<usize>,
}

/// Assigns `members` to `slots` slots. Your own user (`own`) comes first
/// when `show_self`, and is left out entirely otherwise. The others follow
/// in list (join) order, or for [`SortOrder::Speaking`] by `last_spoke`
/// (higher = more recent; missing = never spoke, kept in join order last).
pub fn assign(
    members: &[MemberState],
    own: Option<&str>,
    show_self: bool,
    sort: SortOrder,
    last_spoke: &HashMap<String, u64>,
    slots: usize,
) -> Assignment {
    let own_index = members.iter().position(|m| own == Some(m.user_id.as_str()));
    let mut others: Vec<usize> = (0..members.len())
        .filter(|&i| Some(i) != own_index)
        .collect();
    if sort == SortOrder::Speaking {
        // Stable: ties keep join order.
        others.sort_by_key(|&i| Reverse(last_spoke.get(&members[i].user_id).copied().unwrap_or(0)));
    }
    let mut order: Vec<usize> = own_index.filter(|_| show_self).into_iter().collect();
    order.extend(others);
    let hidden = if order.len() > slots {
        order.split_off(slots)
    } else {
        Vec::new()
    };
    Assignment {
        shown: order,
        hidden,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(id: &str) -> MemberState {
        MemberState {
            user_id: id.into(),
            name: format!("user{id}"),
            avatar: None,
            discriminator: None,
            speaking: false,
            self_mute: false,
            self_deaf: false,
            server_mute: false,
            server_deaf: false,
        }
    }

    fn members(ids: &[&str]) -> Vec<MemberState> {
        ids.iter().map(|id| member(id)).collect()
    }

    /// User ids of shown and hidden members.
    fn ids(members: &[MemberState], a: &Assignment) -> (Vec<String>, Vec<String>) {
        let pick = |v: &[usize]| v.iter().map(|&i| members[i].user_id.clone()).collect();
        (pick(&a.shown), pick(&a.hidden))
    }

    fn run(
        list: &[MemberState],
        show_self: bool,
        sort: SortOrder,
        spoke: &[(&str, u64)],
        slots: usize,
    ) -> (Vec<String>, Vec<String>) {
        let spoke: HashMap<String, u64> = spoke.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        ids(
            list,
            &assign(list, Some("me"), show_self, sort, &spoke, slots),
        )
    }

    #[test]
    fn self_first_or_excluded() {
        // Own user is not necessarily first in the input.
        let list = members(&["a", "me", "b"]);
        assert_eq!(
            run(&list, true, SortOrder::Join, &[], 5),
            (vec!["me".into(), "a".into(), "b".into()], vec![])
        );
        assert_eq!(
            run(&list, false, SortOrder::Join, &[], 5),
            (vec!["a".into(), "b".into()], vec![])
        );
        // Unknown own id: everyone in order.
        let a = assign(&list, None, true, SortOrder::Join, &HashMap::new(), 5);
        assert_eq!(a.shown, vec![0, 1, 2]);
        assert!(
            assign(&[], Some("me"), true, SortOrder::Join, &HashMap::new(), 5)
                .shown
                .is_empty()
        );
    }

    #[test]
    fn speaking_sort_puts_recent_speakers_first_after_self() {
        let list = members(&["me", "a", "b", "c", "d"]);
        let spoke = [("c", 7), ("a", 3), ("me", 9)];
        assert_eq!(
            run(&list, true, SortOrder::Speaking, &spoke, 5).0,
            ["me", "c", "a", "b", "d"]
        );
        assert_eq!(
            run(&list, false, SortOrder::Speaking, &spoke, 5).0,
            ["c", "a", "b", "d"]
        );
        // Join order ignores recency.
        assert_eq!(
            run(&list, true, SortOrder::Join, &spoke, 5).0,
            ["me", "a", "b", "c", "d"]
        );
        // Equal recency keeps join order.
        assert_eq!(
            run(&list, true, SortOrder::Speaking, &[("d", 4), ("b", 4)], 5).0,
            ["me", "b", "d", "a", "c"]
        );
    }

    #[test]
    fn overflow_boundary() {
        let five = members(&["me", "a", "b", "c", "d"]);
        assert_eq!(
            run(&five, true, SortOrder::Join, &[], 5).1,
            Vec::<String>::new()
        );
        let six = members(&["me", "a", "b", "c", "d", "e"]);
        assert_eq!(
            run(&six, true, SortOrder::Join, &[], 5),
            (
                vec!["me".into(), "a".into(), "b".into(), "c".into(), "d".into()],
                vec!["e".into()]
            )
        );
        // Excluding self frees a slot.
        assert!(run(&six, false, SortOrder::Join, &[], 5).1.is_empty());
        // Hidden members follow the sort order too.
        assert_eq!(
            run(&six, true, SortOrder::Speaking, &[("e", 1)], 2),
            (
                vec!["me".into(), "e".into()],
                vec!["a".into(), "b".into(), "c".into(), "d".into()]
            )
        );
        assert_eq!(run(&six, true, SortOrder::Join, &[], 1).1.len(), 5);
    }
}
