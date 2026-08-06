//! A list cursor with its own scroll offset. Two of these back the sidebar's
//! Sessions and Agents sections, which each keep their own position.

pub(crate) struct Pane<T> {
    items: Vec<T>,
    /// Consumed by later sidebar-sections tasks.
    #[allow(dead_code)]
    pub(crate) cursor: usize,
    /// Consumed by later sidebar-sections tasks.
    #[allow(dead_code)]
    pub(crate) offset: usize,
}

impl<T> Pane<T> {
    /// Methods like `items()`, `len()`, `is_empty()`, `selected()`, and `visible_range()`
    /// are consumed by later tasks in the split-sidebar implementation.
    #[allow(dead_code)]
    pub(crate) fn new(items: Vec<T>) -> Self {
        Self {
            items,
            cursor: 0,
            offset: 0,
        }
    }

    #[allow(dead_code)]
    pub(crate) fn items(&self) -> &[T] {
        &self.items
    }

    #[allow(dead_code)]
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    #[allow(dead_code)]
    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[allow(dead_code)]
    pub(crate) fn selected(&self) -> Option<&T> {
        self.items.get(self.cursor)
    }

    #[allow(dead_code)]
    pub(crate) fn move_by(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let last = self.items.len() - 1;
        let next = (self.cursor as isize).saturating_add(delta);
        self.cursor = next.clamp(0, last as isize) as usize;
    }

    /// Replaces the items, keeping the cursor on whichever item matches `keep`
    /// rather than on its old index — the list is rebuilt on every refresh and
    /// rows move as sessions expand, are created, or die.
    #[allow(dead_code)]
    pub(crate) fn set_items<F>(&mut self, items: Vec<T>, keep: Option<&str>, key: F)
    where
        F: Fn(&T) -> &str,
    {
        self.items = items;
        if let Some(keep) = keep {
            if let Some(index) = self.items.iter().position(|item| key(item) == keep) {
                self.cursor = index;
                return;
            }
        }
        self.cursor = self.cursor.min(self.items.len().saturating_sub(1));
    }

    /// Scrolls the minimum distance needed to bring the cursor into a window of
    /// `height` rows.
    pub(crate) fn keep_visible(&mut self, height: usize) {
        if height == 0 {
            self.offset = 0;
            return;
        }
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if self.cursor >= self.offset + height {
            self.offset = self.cursor + 1 - height;
        }
    }

    #[allow(dead_code)]
    pub(crate) fn visible_range(&self, height: usize) -> std::ops::Range<usize> {
        let start = self.offset.min(self.items.len());
        let end = start.saturating_add(height).min(self.items.len());
        start..end
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn move_by_clamps_at_both_ends() {
        let mut pane = Pane::new(vec!["a", "b", "c"]);
        assert_eq!(pane.selected(), Some(&"a"));

        pane.move_by(1);
        assert_eq!(pane.selected(), Some(&"b"));

        pane.move_by(10);
        assert_eq!(pane.selected(), Some(&"c"));

        pane.move_by(-10);
        assert_eq!(pane.selected(), Some(&"a"));
    }

    #[test]
    fn empty_pane_has_no_selection_and_ignores_movement() {
        let mut pane: Pane<&str> = Pane::new(Vec::new());
        pane.move_by(1);
        assert_eq!(pane.selected(), None);
        assert!(pane.is_empty());
    }

    #[test]
    fn keep_visible_scrolls_only_far_enough_to_show_the_cursor() {
        let mut pane = Pane::new((0..10).collect::<Vec<_>>());
        pane.move_by(5);
        pane.keep_visible(3);
        // Cursor 5 with 3 visible rows -> rows 3..6.
        assert_eq!(pane.visible_range(3), 3..6);

        pane.move_by(-5);
        pane.keep_visible(3);
        assert_eq!(pane.visible_range(3), 0..3);
    }

    #[test]
    fn set_items_keeps_the_cursor_on_the_same_key() {
        let mut pane = Pane::new(vec!["a", "b", "c"]);
        pane.move_by(2);
        assert_eq!(pane.selected(), Some(&"c"));

        // "c" moved to the front; the cursor follows it rather than the index.
        pane.set_items(vec!["c", "a", "b"], Some("c"), |item| item);
        assert_eq!(pane.selected(), Some(&"c"));
        assert_eq!(pane.cursor, 0);
    }

    #[test]
    fn set_items_clamps_when_the_key_is_gone() {
        let mut pane = Pane::new(vec!["a", "b", "c"]);
        pane.move_by(2);
        pane.set_items(vec!["a"], Some("c"), |item| item);
        assert_eq!(pane.selected(), Some(&"a"));
        assert_eq!(pane.cursor, 0);
    }
}
