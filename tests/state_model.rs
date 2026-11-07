use spritx_terminal::state::{MAX_PANES_PER_TAB, MAX_SPLIT_RATIO, MIN_SPLIT_RATIO, PaneNode, SplitDirection, TabState};

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
fn split_is_capped_at_five_panes() {
    let mut tab = TabState::single(1, "test", 10);
    assert!(tab.split_leaf(10, 11, SplitDirection::Row));
    assert!(tab.split_leaf(11, 12, SplitDirection::Row));
    assert!(tab.split_leaf(12, 13, SplitDirection::Row));
    assert!(tab.split_leaf(13, 14, SplitDirection::Row));
    assert!(!tab.split_leaf(14, 15, SplitDirection::Row));
    assert_eq!(tab.pane_count(), 5);
    assert!(tab.close_leaf(14));
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

/// Tree: Split(Row, 0.5) over [ Split(Column, 0.5) over [Leaf 10, Leaf 11], Leaf 12 ].
fn nested_tree() -> PaneNode {
    PaneNode::split(
        SplitDirection::Row,
        0.5,
        PaneNode::split(
            SplitDirection::Column,
            0.5,
            PaneNode::leaf(10),
            PaneNode::leaf(11),
        ),
        PaneNode::leaf(12),
    )
}

fn split_ratio(node: &PaneNode) -> f32 {
    match node {
        PaneNode::Split { ratio, .. } => *ratio,
        PaneNode::Leaf(_) => panic!("expected a split node"),
    }
}

#[test]
fn set_ratio_out_of_bounds_path_returns_false() {
    let mut tree = nested_tree();
    // Out-of-range segments used to index the fixed-size children array
    // directly and panic; they must now return false without touching state.
    assert!(!tree.set_ratio(&[2], 0.5), "segment 2 exceeds 2 children");
    assert!(!tree.set_ratio(&[9, 0], 0.5), "segment 9 exceeds 2 children");
    assert!(!tree.set_ratio(&[0, 5], 0.5), "segment 5 exceeds nested split's children");
    assert!(!tree.set_ratio(&[1, 0], 0.5), "a leaf has no children to address");
    assert_eq!(split_ratio(&tree), 0.5, "rejected paths must not move the root ratio");
    // Valid paths still apply: [] addresses the root, [0] the nested split.
    assert!(tree.set_ratio(&[], 0.4));
    assert_eq!(split_ratio(&tree), 0.4);
    assert!(tree.set_ratio(&[0], 0.6));
    let child = match &tree {
        PaneNode::Split { children, .. } => &children[0],
        PaneNode::Leaf(_) => panic!("root must be a split"),
    };
    assert_eq!(split_ratio(child), 0.6);
}

#[test]
fn set_ratio_clamps_to_min_and_max() {
    assert_eq!(MAX_SPLIT_RATIO, 0.9);
    assert_eq!(MIN_SPLIT_RATIO, 0.1);
    let mut tree = nested_tree();
    assert!(tree.set_ratio(&[], 5.0));
    assert_eq!(split_ratio(&tree), MAX_SPLIT_RATIO);
    assert!(tree.set_ratio(&[], -1.0));
    assert_eq!(split_ratio(&tree), MIN_SPLIT_RATIO);
    // An interior value is stored verbatim, not snapped to a bound.
    assert!(tree.set_ratio(&[], 0.55));
    assert_eq!(split_ratio(&tree), 0.55);
    // A nested split clamps independently of the root.
    assert!(tree.set_ratio(&[0], 42.0));
    let child = match &tree {
        PaneNode::Split { children, .. } => &children[0],
        PaneNode::Leaf(_) => panic!("root must be a split"),
    };
    assert_eq!(split_ratio(child), MAX_SPLIT_RATIO);
}

#[test]
fn close_leaf_moves_focus_to_forward_neighbour() {
    let mut tab = TabState::single(1, "test", 10);
    assert!(tab.split_leaf(10, 11, SplitDirection::Row));
    assert!(tab.split_leaf(11, 12, SplitDirection::Row));
    assert_eq!(tab.leaf_ids(), vec![10, 11, 12]);
    // Focus the middle leaf, which is NOT the first leaf in tree order.
    tab.active_leaf = 11;
    assert!(tab.close_leaf(11));
    assert_eq!(tab.leaf_ids(), vec![10, 12]);
    assert_eq!(tab.pane_count(), 2);
    // Legacy code fell back to first_leaf() == 10; the fix must land on
    // the forward tree-order neighbour 12.
    assert_eq!(tab.active_leaf, 12);
    assert_eq!(tab.active_leaf_state().map(|state| state.id), Some(12));
}

#[test]
fn close_leaf_refuses_last_pane_and_unknown_leaf() {
    // The last pane must survive.
    let mut solo = TabState::single(1, "test", 10);
    assert!(!solo.close_leaf(10));
    assert_eq!(solo.pane_count(), 1);
    assert_eq!(solo.leaf_ids(), vec![10]);
    assert_eq!(solo.active_leaf, 10);

    // An unknown leaf changes nothing.
    let mut tab = TabState::single(1, "test", 10);
    assert!(tab.split_leaf(10, 11, SplitDirection::Row));
    let active_before = tab.active_leaf;
    assert_eq!(active_before, 11, "splitting focuses the new leaf");
    assert!(!tab.close_leaf(999));
    assert_eq!(tab.leaf_ids(), vec![10, 11]);
    assert_eq!(tab.pane_count(), 2);
    assert_eq!(tab.active_leaf, active_before);

    // Closing a non-focused pane leaves focus exactly where it was.
    assert!(tab.close_leaf(10));
    assert_eq!(tab.leaf_ids(), vec![11]);
    assert_eq!(tab.active_leaf, 11);
    assert_eq!(tab.active_leaf_state().map(|state| state.id), Some(11));
}

#[test]
fn leaf_ids_follow_tree_order_and_splits_stop_at_cap() {
    assert_eq!(MAX_PANES_PER_TAB, 5);
    let mut tab = TabState::single(1, "test", 30);
    assert!(tab.split_leaf(30, 10, SplitDirection::Row));
    assert!(tab.split_leaf(10, 20, SplitDirection::Row));
    // Tree (DFS) order, not the BTreeMap's sorted order [10, 20, 30].
    assert_eq!(tab.leaf_ids(), vec![30, 10, 20]);
    assert_eq!(tab.pane_count(), 3);
    assert!(tab.split_leaf(20, 40, SplitDirection::Row));
    assert!(tab.split_leaf(40, 50, SplitDirection::Row));
    assert_eq!(tab.pane_count(), MAX_PANES_PER_TAB);
    let ids_before = tab.leaf_ids();
    assert!(!tab.split_leaf(50, 60, SplitDirection::Row), "split past the cap must be refused");
    assert_eq!(tab.pane_count(), MAX_PANES_PER_TAB);
    assert_eq!(tab.leaf_ids(), ids_before);
    assert!(!tab.leaves.contains_key(&60), "no phantom leaf registration on refusal");
    // Splitting an unknown target is likewise refused without side effects.
    assert!(!tab.split_leaf(999, 61, SplitDirection::Row));
    assert!(!tab.leaves.contains_key(&61));
    assert_eq!(tab.pane_count(), MAX_PANES_PER_TAB);
}
#[test]
fn generated_terminal_titles_do_not_duplicate_the_tab_number() {
    use spritx_terminal::state::TabState;
    assert_eq!(TabState::single(13, "Terminal 13", 1).display_title(), "Terminal");
    assert_eq!(TabState::single(42, "Terminal", 1).display_title(), "Terminal");
    assert_eq!(TabState::single(13, "My Terminal 13", 1).display_title(), "My Terminal 13");
    assert_eq!(TabState::single(13, "Terminal 99", 1).display_title(), "Terminal 99");
}
