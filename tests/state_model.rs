use spritx_terminal::state::{PaneNode, SplitDirection, TabState};

#[test]
fn split_and_close_promote_siblings() {
    let mut tab = TabState::single(1, "test", 10);
    assert!(tab.split_leaf(10, 11, SplitDirection::Row));
    assert_eq!(tab.pane_count(), 2);
    assert!(matches!(tab.tree, PaneNode::Split { .. }));
    assert_eq!(tab.leaf_ids(), vec![10, 11]);

    assert!(tab.split_leaf(11, 12, SplitDirection::Column));
    assert_eq!(tab.pane_count(), 3);
    assert!(tab.close_leaf(11));
    assert!(!tab.leaf_ids().contains(&11));
    assert_eq!(tab.pane_count(), 2);

    assert!(tab.close_leaf(12));
    assert_eq!(tab.pane_count(), 1);
    assert!(!tab.close_leaf(10), "the last pane must survive");
}

#[test]
fn split_is_capped_at_four_panes() {
    let mut tab = TabState::single(1, "test", 10);
    assert!(tab.split_leaf(10, 11, SplitDirection::Row));
    assert!(tab.split_leaf(11, 12, SplitDirection::Row));
    assert!(tab.split_leaf(12, 13, SplitDirection::Row));
    assert!(!tab.split_leaf(13, 14, SplitDirection::Row));
    assert_eq!(tab.pane_count(), 4);
}

#[test]
fn split_ratio_is_clamped() {
    let mut tab = TabState::single(1, "test", 10);
    assert!(tab.split_leaf(10, 11, SplitDirection::Column));
    assert!(tab.set_split_ratio(&[], 0.01));
    if let PaneNode::Split { ratio, .. } = tab.tree {
        assert!((0.1..=0.9).contains(&ratio));
    } else {
        panic!("expected split");
    }
}

#[test]
fn neighbour_navigation_walks_leaves_in_order() {
    let mut tab = TabState::single(1, "test", 10);
    assert!(tab.split_leaf(10, 11, SplitDirection::Row));
    assert_eq!(tab.neighbour_leaf(10, true), Some(11));
    assert_eq!(tab.neighbour_leaf(11, false), Some(10));
}
