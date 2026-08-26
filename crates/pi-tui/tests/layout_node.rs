use std::sync::{Arc, Mutex};

use pi_tui::components::{HStack, ScrollView, Text, VStack};
use pi_tui::{
    allocate_stack_sizes, render_layout_frame, visible_stack_entries, Component, LayoutAlign,
    LayoutBasis, LayoutDirection, LayoutNode, LayoutViewport, ScrollbarMode, SharedComponent,
    StackLayoutEntry, StackLayoutNode,
};

fn text(value: &str) -> SharedComponent {
    Arc::new(Mutex::new(Text::new(value, 0, 0, None)))
}

#[test]
fn stack_layout_matches_basis_grow_shrink_and_limits() {
    let entries = vec![
        StackLayoutEntry::new(text("fixed")).with_basis(LayoutBasis::Cells(8)),
        StackLayoutEntry::new(text("grow"))
            .with_grow(2)
            .with_max_size(20),
        StackLayoutEntry::new(text("grow2"))
            .with_grow(1)
            .with_min_size(4),
    ];
    assert_eq!(
        allocate_stack_sizes(&entries, &[8, 1, 4], Some(30), 1),
        vec![8, 14, 6]
    );
    assert_eq!(
        allocate_stack_sizes(&entries, &[8, 20, 20], Some(18), 1),
        vec![1, 5, 10]
    );
}

#[test]
fn visibility_is_evaluated_against_the_current_viewport() {
    let hidden =
        StackLayoutEntry::new(text("hidden")).with_visibility(|viewport| viewport.width > 20);
    let visible = StackLayoutEntry::new(text("visible"));
    let entries = vec![hidden, visible];
    assert_eq!(
        visible_stack_entries(
            &entries,
            LayoutViewport {
                width: 10,
                height: 4
            }
        )
        .len(),
        1
    );
    assert_eq!(
        visible_stack_entries(
            &entries,
            LayoutViewport {
                width: 30,
                height: 4
            }
        )
        .len(),
        2
    );
}

#[test]
fn nested_nodes_render_with_clipped_width_and_nonempty_geometry() {
    let left = text("左側");
    let right = text("right");
    let stack = Arc::new(Mutex::new(HStack::new(vec![(0.0, left), (0.0, right)])));
    let root = Arc::new(Mutex::new(VStack::new(vec![stack])));
    let frame = render_layout_frame(root, 7, 3);
    assert_eq!((frame.width, frame.height), (7, 3));
    assert_eq!(frame.root.rect.width, 7);
    assert!(frame
        .lines
        .iter()
        .all(|line| pi_tui::visible_width(line) <= 7));
    assert!(frame.lines[0].contains('左') || frame.lines[0].contains('r'));
}

#[test]
fn scroll_node_updates_shared_layout_state_and_handles_nested_lookup() {
    let child = text(
        &(1..=12)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let scroll = Arc::new(Mutex::new(ScrollView::with_options(
        child,
        true,
        pi_tui::ScrollOverscroll::Contain,
    )));
    let root: SharedComponent = Arc::new(Mutex::new(VStack::new(vec![scroll.clone()])));
    let frame = render_layout_frame(root, 20, 4);
    let state = frame
        .primary_scroll_view
        .clone()
        .expect("primary scroll state");
    assert_eq!(state.scroll_top(), 8);
    assert!(state.is_following_end());
    assert_eq!(
        pi_tui::get_scroll_view_box(&frame, &state),
        Some(frame.root.children[0].rect)
    );
    assert_eq!(pi_tui::get_scroll_views_at(&frame, 2, 2).len(), 1);
}

#[test]
fn always_scrollbar_reserves_width_paints_thumb_and_handles_narrow_width() {
    let child = text(
        &(1..=12)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let mut view = ScrollView::with_options(child, true, pi_tui::ScrollOverscroll::Contain);
    view.set_scrollbar(ScrollbarMode::Always);
    let scroll = Arc::new(Mutex::new(view));
    let root: SharedComponent = Arc::new(Mutex::new(VStack::new(vec![scroll])));
    let frame = render_layout_frame(root, 8, 3);
    let scroll_box = &frame.root.children[0];
    assert_eq!(scroll_box.rect.width, 8);
    assert_eq!(scroll_box.children[0].rect.width, 7);
    let geometry = pi_tui::get_scrollbar_geometry(scroll_box).expect("scrollbar geometry");
    assert_eq!(geometry.column, 7);
    assert_eq!(geometry.track_top, 0);
    assert_eq!(geometry.track_height, 3);
    assert!(geometry.thumb_height >= 2);
    assert!(frame.lines.iter().any(|line| line.contains("\x1b[100m")));
    assert!(frame
        .lines
        .iter()
        .all(|line| pi_tui::visible_width(line) <= 8));

    let narrow = render_layout_frame(frame.root.component.clone(), 1, 1);
    assert_eq!(narrow.width, 1);
    assert!(narrow
        .lines
        .iter()
        .all(|line| pi_tui::visible_width(line) <= 1));
}

#[test]
fn custom_layout_node_is_public_and_falls_back_safely_at_zero_dimensions() {
    struct EmptyNode;
    impl Component for EmptyNode {
        fn render(&self, _width: usize) -> Vec<String> {
            vec!["0123456789".to_string()]
        }
        fn layout_node(&self) -> Option<LayoutNode> {
            Some(LayoutNode::Stack(StackLayoutNode {
                direction: LayoutDirection::Vertical,
                entries: Vec::new(),
                gap: 100,
                align: LayoutAlign::End,
            }))
        }
    }
    let root: SharedComponent = Arc::new(Mutex::new(EmptyNode));
    let frame = render_layout_frame(root, 0, 0);
    assert_eq!((frame.width, frame.height), (1, 1));
    assert_eq!(frame.lines.len(), 1);
}
