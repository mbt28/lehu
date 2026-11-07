//! List models synced in place by id: a row made from one of the host's
//! records is updated where it stands, erased when its record went,
//! inserted where the host has it when new. Never a reset or a new model:
//! that destroys every row instance (keyboard focus is lost and the whole
//! list repaints).

use slint::{Model, VecModel};

/// What a row is made from: one of the host's records, named by its id.
pub(super) trait Item {
    fn id(&self) -> &str;
}

/// A list row made from a host record, matched by id.
pub(super) trait Listed: Clone + PartialEq + 'static {
    type Item: Item;
    fn from_item(item: &Self::Item, scratch: &mut String) -> Self;
    fn id(&self) -> &str;
}

/// Bring `model` in line with `items`, in their order. A row that moved
/// (a phone that connected goes to the top) is made again where it belongs:
/// a removed row never reports losing focus, so the caller is told when the
/// ring's row -- ring-row `<prefix><id>` -- was erased or made again.
pub(super) fn sync_rows<R: Listed>(
    model: &VecModel<R>,
    items: &[R::Item],
    scratch: &mut String,
    ring: &str,
    prefix: &str,
) -> bool {
    let ringed = ring.strip_prefix(prefix);
    let mut lost = false;
    let mut i = model.row_count();
    while i > 0 {
        i -= 1;
        let Some(row) = model.row_data(i) else {
            continue;
        };
        if !items.iter().any(|item| item.id() == row.id()) {
            lost |= ringed == Some(row.id());
            model.remove(i);
        }
    }
    // The first `i` rows are aligned with the first `i` items by now, so a
    // row with this id can only be at `i` or further down.
    for (i, item) in items.iter().enumerate() {
        let want = R::from_item(item, scratch);
        let at = (i..model.row_count())
            .find(|&j| model.row_data(j).is_some_and(|row| row.id() == want.id()));
        match at {
            None => model.insert(i, want),
            Some(j) if j == i => {
                if model.row_data(i).as_ref() != Some(&want) {
                    model.set_row_data(i, want);
                }
            }
            Some(j) => {
                lost |= ringed == Some(want.id());
                model.remove(j);
                model.insert(i, want);
            }
        }
    }
    lost
}
