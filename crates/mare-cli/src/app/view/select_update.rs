//! An open select's items replaced in place (`UiCmd::SelectUpdate`, the `:update()` of the handle
//! `mare.ui.select{on_open=…}` gives): a list that changes under the user (the session picker's
//! index catching up) keeps the search text, the searching state and the highlighted item —
//! found again by its `item_id`, else by its label — instead of being closed and opened again.

use mare_shared::ui::{SelectItem, SelectTab};

/// What identifies an item across updates: its `item_id`, else its label.
fn key(it: &SelectItem) -> (bool, &str) {
    it.item_id.as_deref().map_or((false, &it.label), |id| (true, id))
}

/// Retain user choices only when their identity is unambiguous in both lists.
fn keep_checks(old: &[SelectItem], new: &mut [SelectItem]) {
    use std::collections::HashMap;

    let mut previous = HashMap::new();
    for item in old {
        previous.entry(key(item)).and_modify(|checked| *checked = None).or_insert(Some(item.checked));
    }
    let mut counts = HashMap::new();
    for item in new.iter() {
        let (has_id, value) = key(item);
        *counts.entry((has_id, value.to_owned())).or_insert(0usize) += 1;
    }
    for item in new {
        let (has_id, value) = key(item);
        if counts.get(&(has_id, value.to_owned())) == Some(&1)
            && let Some(Some(checked)) = previous.get(&(has_id, value))
        {
            item.checked = *checked;
        }
    }
}

/// Take over the lazily loaded preview of the same item when the new one comes without one.
fn keep_previews(old: &mut [SelectItem], new: &mut [SelectItem]) {
    for n in new.iter_mut().filter(|n| n.preview.is_none()) {
        if let Some(o) = old.iter_mut().find(|o| key(o) == key(n)) {
            n.preview = o.preview.take();
        }
    }
}

/// The position in `new` (all items, no filter) of the item at `old[at]`, if it is still there.
fn same_item(old: &[SelectItem], at: Option<usize>, new: &[SelectItem]) -> Option<usize> {
    let k = key(old.get(at?)?);
    new.iter().position(|n| n.is_selectable() && key(n) == k)
}

/// Replace the active list (and, when `new_tabs` is given, every tab) of an open select.
/// `selected` is a position in what the filter shows, and stays on the same item when the item
/// is still shown, else on the nearest selectable item. When `new_tabs` shrinks below `*tab`,
/// `*tab` is clamped into `0..new_tabs.len()` and `items` / `selected` are synced from the
/// newly active tab.
pub(crate) fn apply(
    items: &mut Vec<SelectItem>,
    selected: &mut usize,
    tabs: &mut Vec<SelectTab>,
    tab: &mut usize,
    filter: &str,
    mut new_items: Vec<SelectItem>,
    new_tabs: Vec<SelectTab>,
) {
    use crate::app::{filter_items as visible, skip_unselectable as first_item};
    let prev_tab = *tab;
    let shown = visible(items, filter);
    let current = shown.get(*selected).copied();
    let mut tab_changed = false;
    let mut active_tab_old_items: Option<(Vec<SelectItem>, usize)> = None;
    if !new_tabs.is_empty() {
        let mut new_tabs = new_tabs;
        let next_tab = prev_tab.min(new_tabs.len() - 1);
        tab_changed = next_tab != prev_tab;
        for (i, nt) in new_tabs.iter_mut().enumerate() {
            if i == prev_tab {
                if tab_changed {
                    let old_pos = current;
                    if let Some(p) = same_item(items, old_pos, &nt.items) {
                        nt.selected = p;
                    }
                    keep_checks(items, &mut nt.items);
                    keep_previews(items, &mut nt.items);
                }
                continue;
            }
            if let Some(old) = tabs.get_mut(i) {
                if tab_changed && i == next_tab {
                    active_tab_old_items = Some((std::mem::take(&mut old.items), old.selected));
                    continue;
                }
                // an inactive tab: its selection is a position in its (unfiltered) items
                if let Some(p) = same_item(&old.items, Some(old.selected), &nt.items) {
                    nt.selected = p;
                }
                keep_checks(&old.items, &mut nt.items);
                keep_previews(&mut old.items, &mut nt.items);
            }
        }
        *tab = next_tab;
        if let Some(active) = new_tabs.get_mut(*tab) {
            new_items = std::mem::take(&mut active.items);
            if tab_changed {
                *selected = active.selected;
            }
        }
        *tabs = new_tabs;
    }
    let at = if let Some((mut prev_items, prev_sel)) = active_tab_old_items {
        let at = same_item(&prev_items, Some(prev_sel), &new_items);
        keep_checks(&prev_items, &mut new_items);
        keep_previews(&mut prev_items, &mut new_items);
        at
    } else if !tab_changed {
        let at = same_item(items, current, &new_items);
        keep_checks(items, &mut new_items);
        keep_previews(items, &mut new_items);
        at
    } else {
        None
    };
    *items = new_items;
    let shown = visible(items, filter);
    let pos = at
        .and_then(|i| shown.iter().position(|&v| v == i))
        .unwrap_or_else(|| (*selected).min(shown.len().saturating_sub(1)));
    *selected = first_item(items, &shown, pos, true);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(item_id: &str, label: &str) -> SelectItem {
        SelectItem { item_id: Some(item_id.into()), label: label.into(), ..Default::default() }
    }

    #[test]
    fn the_highlighted_item_is_found_again_by_id_under_the_same_filter() {
        use crate::app::filter_items;
        let on = |items: &[SelectItem], selected: usize| items[filter_items(items, "a")[selected]].label.clone();
        let mut items = vec![item("a", "alpha"), item("b", "beta"), item("c", "gamma")];
        // the search "a" shows all three (best match first); the user is on beta
        let mut selected = filter_items(&items, "a").iter().position(|&i| i == 1).unwrap();
        let new = vec![item("n", "a new one"), item("c", "gamma"), item("b", "beta (renamed)")];
        apply(&mut items, &mut selected, &mut vec![], &mut 0, "a", new, vec![]);
        assert_eq!(on(&items, selected), "beta (renamed)");
        // the item is gone: the same position, clamped to what is shown
        let mut selected = 2;
        apply(&mut items, &mut selected, &mut vec![], &mut 0, "a", vec![item("x", "a")], vec![]);
        assert_eq!(selected, 0);
    }

    #[test]
    fn tabs_keep_their_selection_and_the_active_tab_its_items() {
        let tab = |name: &str, items: Vec<SelectItem>, selected| SelectTab { name: name.into(), items, selected };
        let mut items = vec![item("1", "one"), item("2", "two")];
        let mut tabs = vec![tab("A", vec![], 0), tab("B", vec![item("x", "x"), item("y", "y")], 1)];
        let mut selected = 1;
        let mut active_tab = 0;
        let new_tabs = vec![
            tab("A", vec![item("0", "zero"), item("1", "one"), item("2", "two")], 0),
            tab("B", vec![item("w", "w"), item("x", "x"), item("y", "y")], 0),
        ];
        apply(&mut items, &mut selected, &mut tabs, &mut active_tab, "", vec![], new_tabs);
        assert_eq!(active_tab, 0);
        assert_eq!(items.len(), 3);
        assert_eq!(items[selected].label, "two");
        assert_eq!(tabs[1].selected, 2, "still on y");
        assert!(tabs[0].items.is_empty(), "the active tab's items live in `items`");
    }

    #[test]
    fn shrinking_tabs_clamps_active_tab_and_syncs_items_without_panicking() {
        let tab = |name: &str, items: Vec<SelectItem>, selected| SelectTab { name: name.into(), items, selected };
        let mut items = vec![item("c1", "tabC item")];
        let mut tabs = vec![
            tab("A", vec![item("a1", "alpha"), item("a2", "beta")], 1),
            tab("B", vec![item("b1", "b")], 0),
            tab("C", vec![], 0),
        ];
        let mut selected = 0;
        let mut active_tab = 2;
        let shrunk = vec![tab("A", vec![item("a0", "zero"), item("a2", "beta")], 0)];
        apply(&mut items, &mut selected, &mut tabs, &mut active_tab, "", vec![], shrunk);
        assert_eq!(active_tab, 0);
        assert_eq!(tabs.len(), 1);
        assert_eq!(items[selected].label, "beta");
    }
}
