use crate::config::{SidebarPosition, StackDirection};
use crate::niri::NiriClient;
use crate::state::save_state;
use crate::window_rules::{resolve_rule_focus_peek, resolve_rule_peek, resolve_window_size};
use crate::{Ctx, WindowTarget};
use anyhow::Result;
use niri_ipc::{Action, PositionChange, Window};
use std::collections::{HashMap, HashSet};

fn resolve_dimensions<C: NiriClient>(window: &Window, ctx: &Ctx<C>) -> WindowTarget {
    let (width, height) = resolve_window_size(
        &ctx.config.window_rule,
        window,
        ctx.config.geometry.width,
        ctx.config.geometry.height,
    );

    // Apps with a minimum size ignore smaller targets, so stack by the size they actually have
    let (actual_width, actual_height) = window.layout.window_size;
    WindowTarget {
        width: width.max(actual_width),
        height: height.max(actual_height),
    }
}

fn calculate_coordinates<C: NiriClient>(
    pos: SidebarPosition,
    dims: WindowTarget,
    screen: (i32, i32),
    stack_offset: i32,
    active_peek: i32,
    focus_peek: i32,
    ctx: &Ctx<C>,
) -> (i32, i32) {
    let state = &ctx.state;
    let margins = &ctx.config.margins;
    let (sw, sh) = screen;
    let (w, h) = (dims.width, dims.height);
    let stack_direction = ctx.config.interaction.get_stack_direction();
    // A focus peek as wide as the window reveals it fully; use that same gap when shown so
    // the focused-hidden and shown positions coincide. Otherwise fall back to the margin.
    let edge_offset = |size: i32, margin: i32| {
        if focus_peek >= size {
            focus_peek - size
        } else {
            margin
        }
    };

    match pos {
        SidebarPosition::Right => {
            let visible_x = sw - w - edge_offset(w, margins.right);
            let hidden_x = sw - active_peek;
            let x = if state.is_hidden { hidden_x } else { visible_x };

            let y = match stack_direction {
                StackDirection::TopToBottom => margins.top + stack_offset,
                StackDirection::BottomToTop => sh - h - margins.bottom - stack_offset,
                _ => unreachable!(
                    "stack direction should already be normalized for sidebar position"
                ),
            };
            (x, y)
        }
        SidebarPosition::Left => {
            let visible_x = edge_offset(w, margins.left);
            let hidden_x = -w + active_peek;
            let x = if state.is_hidden { hidden_x } else { visible_x };

            let y = match stack_direction {
                StackDirection::TopToBottom => margins.top + stack_offset,
                StackDirection::BottomToTop => sh - h - margins.bottom - stack_offset,
                _ => unreachable!(
                    "stack direction should already be normalized for sidebar position"
                ),
            };
            (x, y)
        }
        SidebarPosition::Bottom => {
            let x = match stack_direction {
                StackDirection::LeftToRight => margins.left + stack_offset,
                StackDirection::RightToLeft => sw - w - margins.right - stack_offset,
                _ => unreachable!(
                    "stack direction should already be normalized for sidebar position"
                ),
            };

            let visible_y = sh - h - edge_offset(h, margins.bottom);
            let hidden_y = sh - active_peek;
            let y = if state.is_hidden { hidden_y } else { visible_y };
            (x, y)
        }
        SidebarPosition::Top => {
            let x = match stack_direction {
                StackDirection::LeftToRight => margins.left + stack_offset,
                StackDirection::RightToLeft => sw - w - margins.right - stack_offset,
                _ => unreachable!(
                    "stack direction should already be normalized for sidebar position"
                ),
            };

            let visible_y = edge_offset(h, margins.top);
            let hidden_y = -h + active_peek;
            let y = if state.is_hidden { hidden_y } else { visible_y };
            (x, y)
        }
    }
}

pub fn reorder<C: NiriClient>(ctx: &mut Ctx<C>) -> Result<()> {
    reconcile_positions(ctx).map(|_| ())
}

pub(crate) fn reconcile_positions<C: NiriClient>(ctx: &mut Ctx<C>) -> Result<bool> {
    let all_windows = ctx.socket.get_windows()?;
    let active_workspaces: HashSet<u64> = ctx
        .socket
        .get_workspaces()?
        .into_iter()
        .filter(|workspace| workspace.is_active)
        .map(|workspace| workspace.id)
        .collect();

    let initial_len = ctx.state.windows.len();
    let active_ids: HashSet<u64> = all_windows.iter().map(|w| w.id).collect();

    ctx.state.windows.retain(|w| active_ids.contains(&w.id));
    ctx.state
        .window_workspaces
        .retain(|id, _| active_ids.contains(id));
    let order_changed = append_transferred_windows(&mut ctx.state, &all_windows);
    if ctx.state.windows.len() != initial_len || order_changed {
        save_state(&ctx.state, &ctx.cache_dir)?;
    }
    let sidebar_ids: Vec<u64> = ctx.state.windows.iter().map(|w| w.id).collect();

    let position = ctx.config.interaction.position;
    let gap = ctx.config.geometry.gap;
    let mut changed = false;

    for workspace_id in active_workspaces {
        let (display_w, display_h) = ctx.socket.get_workspace_dimensions(workspace_id)?;
        let mut sidebar_windows: Vec<_> = all_windows
            .iter()
            .filter(|window| {
                window.is_floating
                    && window.workspace_id == Some(workspace_id)
                    && sidebar_ids.contains(&window.id)
            })
            .collect();
        sidebar_windows.sort_by_key(|window| {
            sidebar_ids
                .iter()
                .position(|id| *id == window.id)
                .unwrap_or(usize::MAX)
        });
        if ctx.state.is_flipped {
            sidebar_windows.reverse();
        }

        let mut current_stack_offset = 0;
        for window in sidebar_windows {
            let dims = resolve_dimensions(window, ctx);
            let window_focus_peek = resolve_rule_focus_peek(
                &ctx.config.window_rule,
                window,
                ctx.config.interaction.get_focus_peek(),
            );
            let active_peek = if window.is_focused || ctx.state.revealed == Some(window.id) {
                window_focus_peek
            } else {
                resolve_rule_peek(&ctx.config.window_rule, window, ctx.config.interaction.peek)
            };
            let (target_x, target_y) = calculate_coordinates(
                position,
                dims,
                (display_w, display_h),
                current_stack_offset,
                active_peek,
                window_focus_peek,
                ctx,
            );
            let tile_size = window.layout.tile_size;
            let clamp_size = (
                if tile_size.0 > 0.0 {
                    tile_size.0
                } else {
                    f64::from(dims.width)
                },
                if tile_size.1 > 0.0 {
                    tile_size.1
                } else {
                    f64::from(dims.height)
                },
            );
            let (effective_x, effective_y) =
                clamp_floating_position((target_x, target_y), (display_w, display_h), clamp_size);

            match position {
                SidebarPosition::Left | SidebarPosition::Right => {
                    current_stack_offset += dims.height + gap;
                }
                SidebarPosition::Top | SidebarPosition::Bottom => {
                    current_stack_offset += dims.width + gap;
                }
            }

            let position_matches =
                window
                    .layout
                    .tile_pos_in_workspace_view
                    .is_some_and(|(x, y)| {
                        (x - f64::from(effective_x)).abs() < 0.5
                            && (y - f64::from(effective_y)).abs() < 0.5
                    });
            if position_matches {
                continue;
            }

            let _ = ctx.socket.send_action(Action::MoveFloatingWindow {
                id: Some(window.id),
                x: PositionChange::SetFixed(target_x.into()),
                y: PositionChange::SetFixed(target_y.into()),
            });
            changed = true;
        }
    }

    Ok(changed)
}

fn append_transferred_windows(state: &mut crate::state::AppState, windows: &[Window]) -> bool {
    let actual_workspaces: HashMap<u64, u64> = windows
        .iter()
        .filter_map(|window| window.workspace_id.map(|workspace| (window.id, workspace)))
        .collect();
    let tracked_ids: HashSet<u64> = state.windows.iter().map(|window| window.id).collect();
    let mut changed_destinations: HashMap<u64, u64> = HashMap::new();
    let mut state_changed = false;

    for (&id, &workspace) in &actual_workspaces {
        if !tracked_ids.contains(&id) {
            continue;
        }
        let previous = state.window_workspaces.get(&id).copied();
        if previous.is_some_and(|previous| previous != workspace) {
            changed_destinations.insert(id, workspace);
        }
        state_changed |= previous != Some(workspace);
        state.window_workspaces.insert(id, workspace);
    }
    let workspace_count = state.window_workspaces.len();
    state
        .window_workspaces
        .retain(|id, _| tracked_ids.contains(id));
    state_changed |= workspace_count != state.window_workspaces.len();

    if changed_destinations.is_empty() {
        return state_changed;
    }

    let mut effective = std::mem::take(&mut state.windows);
    if state.is_flipped {
        effective.reverse();
    }
    let mut destinations: Vec<u64> = changed_destinations.values().copied().collect();
    destinations.sort_unstable();
    destinations.dedup();

    for destination in destinations {
        let mut moved = Vec::new();
        let mut remaining = Vec::with_capacity(effective.len());
        for window in effective {
            if changed_destinations.get(&window.id) == Some(&destination) {
                moved.push(window);
            } else {
                remaining.push(window);
            }
        }
        let insertion = remaining
            .iter()
            .rposition(|window| actual_workspaces.get(&window.id) == Some(&destination))
            .map_or(remaining.len(), |index| index + 1);
        remaining.splice(insertion..insertion, moved);
        effective = remaining;
    }

    if state.is_flipped {
        effective.reverse();
    }
    state.windows = effective;
    true
}

fn clamp_floating_position(
    (x, y): (i32, i32),
    (display_w, display_h): (i32, i32),
    (tile_w, tile_h): (f64, f64),
) -> (i32, i32) {
    let min_visible_x = (tile_w / 4.0).clamp(10.0, 75.0);
    let min_visible_y = (tile_h / 4.0).clamp(10.0, 75.0);
    let min_x = -tile_w + min_visible_x;
    let max_x = f64::from(display_w) - min_visible_x;
    let min_y = -tile_h + min_visible_y;
    let max_y = f64::from(display_h) - min_visible_y;

    (
        f64::from(x).clamp(min_x, max_x).round() as i32,
        f64::from(y).clamp(min_y, max_y).round() as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WindowRule;
    use crate::state::{AppState, WindowState};
    use crate::test_utils::{MockNiri, mock_config, mock_window, mock_workspace};
    use niri_ipc::{Action, PositionChange};
    use regex::Regex;
    use tempfile::tempdir;

    #[test]
    fn test_standard_stacking_order() {
        let temp_dir = tempdir().unwrap();
        // Scenario: Two windows, visible. Check Y-axis stacking.
        let w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let w2 = mock_window(2, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w1, w2]);

        let mut state = AppState::default();
        // 1 is bottom, 2 is top
        let w1 = WindowState {
            id: 1,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        };
        let w2 = WindowState {
            id: 2,
            width: 300,
            height: 200,
            is_floating: true,
            position: Some((1.0, 2.0)),
        };
        state.windows.push(w1);
        state.windows.push(w2);

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        let actions = &ctx.socket.sent_actions;
        assert_eq!(actions.len(), 2);

        // Screen W: 1920, H: 1080
        // Config: W: 300, H: 200, Gap: 10, Top: 50, Right: 20
        let base_x = 1920 - 300 - 20; // 1600
        let base_y = 1080 - 200 - 50; // 830 (Bottom-most slot)

        // Window 1 (Index 0)
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow {
                id: Some(1),
                x: PositionChange::SetFixed(x),
                y: PositionChange::SetFixed(y)
            } if *x == f64::from(base_x) && *y == f64::from(base_y)
        )));

        // Window 2 (Index 1) -> Stacked above
        // Y = BaseY - (Height + Gap) = 830 - (200 + 10) = 620
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow {
                id: Some(2),
                x: PositionChange::SetFixed(x),
                y: PositionChange::SetFixed(y)
            } if *x == f64::from(base_x) && *y == 620.0
        )));
    }

    #[test]
    fn test_hidden_mode_with_focus_peek() {
        let temp_dir = tempdir().unwrap();
        // Scenario: Hidden mode. Focused window should stick out more.
        let w_focused = mock_window(1, true, true, 1, Some((1.0, 2.0)));
        let w_bg = mock_window(2, false, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w_focused, w_bg]);

        let mut state = AppState {
            is_hidden: true,
            ..Default::default()
        };
        let w1 = WindowState {
            id: 1,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        };
        let w2 = WindowState {
            id: 2,
            width: 300,
            height: 200,
            is_floating: true,
            position: Some((1.0, 2.0)),
        };
        state.windows.push(w1);
        state.windows.push(w2);

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        let actions = &ctx.socket.sent_actions;

        // Config: Peek: 10, FocusPeek: 50
        // 1. Unfocused Window (ID 2) -> Should be at 1920 - 10 = 1910
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(2), x: PositionChange::SetFixed(x), .. }
            if *x == 1910.0
        )));

        // 2. Focused Window (ID 1) -> Should be at 1920 - 50 = 1870
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(1), x: PositionChange::SetFixed(x), .. }
            if *x == 1870.0
        )));
    }

    #[test]
    fn test_reorder_skips_position_already_clamped_by_niri() {
        let temp_dir = tempdir().unwrap();
        let window = mock_window(1, false, true, 1, Some((1845.0, 830.0)));
        let mock = MockNiri::new(vec![window]);
        let mut state = AppState {
            is_hidden: true,
            ..Default::default()
        };
        state.windows.push(WindowState {
            id: 1,
            width: 300,
            height: 200,
            is_floating: true,
            position: Some((1845.0, 830.0)),
        });
        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        assert!(ctx.socket.sent_actions.is_empty());
    }

    #[test]
    fn test_reorder_stacks_each_active_workspace_independently() {
        let temp_dir = tempdir().unwrap();
        let first = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let second = mock_window(2, false, true, 2, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![first, second]).with_workspaces(vec![
            mock_workspace(1, "DP-1", true, true),
            mock_workspace(2, "DP-2", true, false),
        ]);
        let mut state = AppState::default();
        for id in [1, 2] {
            state.windows.push(WindowState {
                id,
                width: 300,
                height: 200,
                is_floating: true,
                position: Some((1.0, 2.0)),
            });
        }
        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        assert_eq!(ctx.socket.sent_actions.len(), 2);
        for id in [1, 2] {
            assert!(ctx.socket.sent_actions.iter().any(|action| matches!(
                action,
                Action::MoveFloatingWindow {
                    id: Some(action_id),
                    x: PositionChange::SetFixed(1600.0),
                    y: PositionChange::SetFixed(830.0),
                } if *action_id == id
            )));
        }
    }

    #[test]
    fn test_transferred_window_appends_to_destination_order() {
        let mut state = AppState::default();
        for id in [1, 2, 3, 4] {
            state.windows.push(WindowState {
                id,
                width: 100,
                height: 100,
                is_floating: true,
                position: None,
            });
        }
        state.window_workspaces = HashMap::from([(1, 1), (2, 2), (3, 2), (4, 1)]);
        let windows = vec![
            mock_window(1, false, true, 2, None),
            mock_window(2, false, true, 2, None),
            mock_window(3, false, true, 2, None),
            mock_window(4, false, true, 1, None),
        ];

        assert!(append_transferred_windows(&mut state, &windows));

        let destination_order: Vec<u64> = state
            .windows
            .iter()
            .filter(|window| state.window_workspaces.get(&window.id) == Some(&2))
            .map(|window| window.id)
            .collect();
        assert_eq!(destination_order, vec![2, 3, 1]);
    }

    #[test]
    fn test_transferred_window_appends_when_order_is_flipped() {
        let mut state = AppState {
            is_flipped: true,
            ..Default::default()
        };
        // Stored order is reversed when displayed.
        for id in [4, 3, 2, 1] {
            state.windows.push(WindowState {
                id,
                width: 100,
                height: 100,
                is_floating: true,
                position: None,
            });
        }
        state.window_workspaces = HashMap::from([(1, 1), (2, 2), (3, 2), (4, 1)]);
        let windows = vec![
            mock_window(1, false, true, 2, None),
            mock_window(2, false, true, 2, None),
            mock_window(3, false, true, 2, None),
            mock_window(4, false, true, 1, None),
        ];

        assert!(append_transferred_windows(&mut state, &windows));

        let destination_order: Vec<u64> = state
            .windows
            .iter()
            .rev()
            .filter(|window| state.window_workspaces.get(&window.id) == Some(&2))
            .map(|window| window.id)
            .collect();
        assert_eq!(destination_order, vec![2, 3, 1]);
    }

    #[test]
    fn test_filters_wrong_workspace_and_cleanup_zombies() {
        let temp_dir = tempdir().unwrap();
        // Scenario:
        // - Window 1: On workspace 1 (Correct)
        // - Window 2: On workspace 99 (Should be ignored)
        // - Window 3: In State, but does not exist in Niri

        let w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let w2 = mock_window(2, false, true, 99, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w1, w2]);

        let mut state = AppState::default();
        let w1 = WindowState {
            id: 1,
            width: 100,
            height: 100,
            is_floating: false,
            position: None,
        };
        let w2 = WindowState {
            id: 2,
            width: 100,
            height: 100,
            is_floating: true,
            position: Some((1.0, 2.0)),
        };
        let w3 = WindowState {
            id: 3,
            width: 100,
            height: 100,
            is_floating: true,
            position: Some((1.0, 2.0)),
        };
        state.windows.push(w1);
        state.windows.push(w2);
        state.windows.push(w3);

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).unwrap();

        // Check Logic:
        // 1. Window 3 should be removed from state
        // 2. Window 2 should NOT be moved
        // 3. Window 1 SHOULD be moved

        let ids: Vec<u64> = ctx.state.windows.iter().map(|w| w.id).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
        assert!(
            !ids.contains(&3),
            "Zombie window 3 should be removed from state"
        );

        // Assert Actions
        let actions = &ctx.socket.sent_actions;

        // Should move ID 1
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::MoveFloatingWindow { id: Some(1), .. }))
        );
        // Should NOT move ID 2 (Wrong WS)
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::MoveFloatingWindow { id: Some(2), .. }))
        );
        // Should NOT move ID 3 (Doesn't exist)
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::MoveFloatingWindow { id: Some(3), .. }))
        );
    }

    #[test]
    fn test_flipped_order() {
        let temp_dir = tempdir().unwrap();
        // Scenario: Flipped mode reverses the visual stack
        let w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let w2 = mock_window(2, false, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w1, w2]);

        let mut state = AppState {
            is_flipped: true,
            ..Default::default()
        };
        let w1 = WindowState {
            id: 1,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        };
        let w2 = WindowState {
            id: 2,
            width: 300,
            height: 200,
            is_floating: true,
            position: Some((1.0, 2.0)),
        };
        state.windows.push(w1);
        state.windows.push(w2);

        let mut ctx = Ctx {
            state,
            config: mock_config(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).unwrap();

        let actions = &ctx.socket.sent_actions;

        // Normal Order: 1 is bottom (idx 0), 2 is top (idx 1)
        // Flipped Order: 2 becomes bottom (idx 0), 1 becomes top (idx 1)
        // Check Window 2 is now at the Bottom (BaseY)
        // BaseY = 1080 - 200 - 50 = 830
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(2), y: PositionChange::SetFixed(y), .. }
            if *y == 830.0
        )));
        // Check Window 1 is now stacked above
        // Y = 830 - (200 + 10) = 620
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(1), y: PositionChange::SetFixed(y), .. }
            if *y == 620.0
        )));
    }

    #[test]
    fn test_position_left_hidden() {
        let temp_dir = tempdir().unwrap();
        // Scenario: Left side, Hidden.
        // Window Width: 300. Peek: 10.
        // Expected X = -300 + 10 = -290.
        let w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w1]);

        let mut config = mock_config();
        config.interaction.position = SidebarPosition::Left;
        config.interaction.peek = 10;
        config.geometry.width = 300;
        config.margins.left = 0;

        let mut state = AppState {
            is_hidden: true,
            ..Default::default()
        };

        let w1 = WindowState {
            id: 1,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        };
        state.windows.push(w1);

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow {
                id: Some(1),
                x: PositionChange::SetFixed(x),
                ..
            } if *x == -290.0 // Verify negative coordinate
        )));
    }

    #[test]
    fn test_position_bottom_stacking() {
        let temp_dir = tempdir().unwrap();
        // Scenario: Bottom bar.
        // Windows should stack Left-to-Right (X axis changes, Y is fixed).
        let w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let w2 = mock_window(2, false, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w1, w2]);

        let mut config = mock_config();
        config.interaction.position = SidebarPosition::Bottom;
        config.geometry.width = 100;
        config.geometry.gap = 10;
        config.margins.left = 20;

        let mut state = AppState::default();

        let w1 = WindowState {
            id: 1,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        };
        let w2 = WindowState {
            id: 2,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        };
        state.windows.push(w1);
        state.windows.push(w2);

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        let actions = &ctx.socket.sent_actions;

        // Window 1 (First): X = Margin Left = 20
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(1), x: PositionChange::SetFixed(x), .. }
            if *x == 20.0
        )));

        // Window 2 (Second): X = Margin + Width + Gap = 20 + 100 + 10 = 130
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow { id: Some(2), x: PositionChange::SetFixed(x), .. }
            if *x == 130.0
        )));
    }

    #[test]
    fn test_top_to_bottom_stacking_order() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let w2 = mock_window(2, false, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w1, w2]);

        let mut config = mock_config();
        config.interaction.stack_direction = StackDirection::TopToBottom;

        let mut state = AppState::default();
        state.windows.push(WindowState {
            id: 1,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        });
        state.windows.push(WindowState {
            id: 2,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        });

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::MoveFloatingWindow {
                id: Some(1),
                x: PositionChange::SetFixed(1600.0),
                y: PositionChange::SetFixed(50.0)
            }
        )));
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::MoveFloatingWindow {
                id: Some(2),
                x: PositionChange::SetFixed(1600.0),
                y: PositionChange::SetFixed(260.0)
            }
        )));
    }

    #[test]
    fn test_top_to_bottom_uses_window_heights_for_offsets() {
        let temp_dir = tempdir().unwrap();
        let w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let mut w2 = mock_window(2, false, true, 1, Some((1.0, 2.0)));
        w2.app_id = Some("tall".into());
        let mock = MockNiri::new(vec![w1, w2]);

        let mut config = mock_config();
        config.interaction.stack_direction = StackDirection::TopToBottom;
        config.window_rule = vec![WindowRule {
            app_id: Some(Regex::new("tall").unwrap()),
            height: Some(400),
            ..Default::default()
        }];

        let mut state = AppState::default();
        state.windows.push(WindowState {
            id: 1,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        });
        state.windows.push(WindowState {
            id: 2,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        });

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::MoveFloatingWindow {
                id: Some(1),
                y: PositionChange::SetFixed(50.0),
                ..
            }
        )));
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::MoveFloatingWindow {
                id: Some(2),
                y: PositionChange::SetFixed(260.0),
                ..
            }
        )));
    }

    #[test]
    fn test_stacking_uses_actual_size_when_window_enforces_minimum() {
        let temp_dir = tempdir().unwrap();
        let mut w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        w1.layout.window_size = (300, 500);
        let w2 = mock_window(2, false, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w1, w2]);

        let mut config = mock_config();
        config.interaction.stack_direction = StackDirection::TopToBottom;

        let mut state = AppState::default();
        for id in [1, 2] {
            state.windows.push(WindowState {
                id,
                width: 300,
                height: 200,
                is_floating: false,
                position: None,
            });
        }

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        // Window 1 is stuck at 500px tall even though 200 was requested: top margin 50 + 500 + gap 10
        assert!(ctx.socket.sent_actions.iter().any(|a| matches!(
            a,
            Action::MoveFloatingWindow {
                id: Some(2),
                y: PositionChange::SetFixed(560.0),
                ..
            }
        )));
    }

    #[test]
    fn test_shown_and_focus_peek_positions_match_when_focus_peek_reveals_window() {
        let x_for = |hidden: bool, focused: bool| {
            let temp_dir = tempdir().unwrap();
            let mock = MockNiri::new(vec![mock_window(1, focused, true, 1, Some((1.0, 2.0)))]);
            let mut config = mock_config();
            config.interaction.position = SidebarPosition::Right;
            config.geometry.width = 300;
            config.interaction.focus_peek = Some(310);
            let mut state = AppState::default();
            state.is_hidden = hidden;
            state.windows.push(WindowState {
                id: 1,
                width: 300,
                height: 200,
                is_floating: false,
                position: None,
            });
            let mut ctx = Ctx {
                state,
                config,
                socket: mock,
                cache_dir: temp_dir.path().to_path_buf(),
            };
            reorder(&mut ctx).expect("Reorder failed");
            ctx.socket
                .sent_actions
                .iter()
                .find_map(|a| match a {
                    Action::MoveFloatingWindow {
                        x: PositionChange::SetFixed(x),
                        ..
                    } => Some(*x),
                    _ => None,
                })
                .unwrap()
        };

        // Screen width 1920: 1920 - 310 (focus peek), not 1920 - 300 - 20 (margin)
        assert_eq!(x_for(false, false), 1610.0);
        assert_eq!(x_for(true, true), 1610.0);
    }

    #[test]
    fn test_revealed_window_uses_focus_peek_while_hidden() {
        let temp_dir = tempdir().unwrap();
        let mock = MockNiri::new(vec![mock_window(1, false, true, 1, Some((1.0, 2.0)))]);
        let mut config = mock_config();
        config.interaction.position = SidebarPosition::Right;
        config.interaction.peek = 10;
        config.interaction.focus_peek = Some(100);
        let mut state = AppState {
            is_hidden: true,
            revealed: Some(1),
            ..Default::default()
        };
        state.windows.push(WindowState {
            id: 1,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        });
        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        // Screen width 1920: revealed with focus peek 100, not the 10px peek
        assert!(ctx.socket.sent_actions.iter().any(|a| matches!(
            a,
            Action::MoveFloatingWindow {
                id: Some(1),
                x: PositionChange::SetFixed(1820.0),
                ..
            }
        )));
    }

    #[test]
    fn test_window_rules_override_behavior() {
        let temp_dir = tempdir().unwrap();
        // Scenario: Two windows. One with a rule, one default.
        // Window 1: Default (Width 300, Peek 10)
        // Window 2: Rule (Width 500, Peek 100)
        let w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let mut w2 = mock_window(2, false, true, 1, Some((1.0, 2.0)));
        w2.app_id = Some("special".into());

        let mock = MockNiri::new(vec![w1, w2]);

        let mut config = mock_config();
        config.interaction.position = SidebarPosition::Right;
        config.geometry.width = 300;
        config.interaction.peek = 10;

        config.window_rule = vec![WindowRule {
            app_id: Some(Regex::new("special").unwrap()),
            width: Some(500),
            peek: Some(100),
            ..Default::default()
        }];

        let mut state = AppState::default();

        // 1 is bottom, 2 is top
        let w1 = WindowState {
            id: 1,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        };
        let w2 = WindowState {
            id: 2,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        };
        state.windows.push(w1);
        state.windows.push(w2);

        state.is_hidden = true;

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        let actions = &ctx.socket.sent_actions;

        // Screen W: 1920

        // Window 1 (Default):
        // Width 300. Peek 10.
        // Hidden X = ScreenW - Peek = 1920 - 10 = 1910
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow {
                id: Some(1),
                x: PositionChange::SetFixed(x),
                ..
            } if *x == 1910.0
        )));

        // Window 2 (Special Rule):
        // Width 500. Peek 100.
        // Hidden X = ScreenW - Peek = 1920 - 100 = 1820
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow {
                id: Some(2),
                x: PositionChange::SetFixed(x),
                ..
            } if *x == 1820.0
        )));
    }

    #[test]
    fn test_window_rules_left_hidden_mixed() {
        let temp_dir = tempdir().unwrap();
        // Scenario: Left side, Hidden.
        // Window 1: Default (Width 300, Peek 10)
        // Window 2: Special (Width 400, Peek 50)
        let w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let mut w2 = mock_window(2, false, true, 1, Some((1.0, 2.0)));
        w2.app_id = Some("special".into());

        let mock = MockNiri::new(vec![w1, w2]);

        let mut config = mock_config();
        config.interaction.position = SidebarPosition::Left;
        config.interaction.peek = 10;
        config.geometry.width = 300;
        config.margins.left = 0;

        config.window_rule = vec![WindowRule {
            app_id: Some(Regex::new("special").unwrap()),
            width: Some(400),
            peek: Some(50),
            ..Default::default()
        }];

        let mut state = AppState::default();

        let w1 = WindowState {
            id: 1,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        };
        let w2 = WindowState {
            id: 2,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        };
        state.windows.push(w1);
        state.windows.push(w2);

        state.is_hidden = true;

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        let actions = &ctx.socket.sent_actions;

        // Window 1 (Default):
        // X = -Width + Peek = -300 + 10 = -290
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow {
                id: Some(1),
                x: PositionChange::SetFixed(x),
                ..
            } if *x == -290.0
        )));

        // Window 2 (Special):
        // X = -Width + Peek = -400 + 50 = -350
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow {
                id: Some(2),
                x: PositionChange::SetFixed(x),
                ..
            } if *x == -350.0
        )));
    }

    #[test]
    fn test_window_rules_bottom_visible_mixed() {
        let temp_dir = tempdir().unwrap();
        // Scenario: Bottom side, Visible.
        // Window 1: Special (Width 200)
        // Window 2: Default (Width 100)
        let mut w1 = mock_window(2, false, true, 1, Some((1.0, 2.0)));
        let w2 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        w1.app_id = Some("wide".into());

        let mock = MockNiri::new(vec![w1, w2]);

        let mut config = mock_config();
        config.interaction.position = SidebarPosition::Bottom;
        config.geometry.width = 100;
        config.geometry.gap = 10;
        config.margins.left = 0;

        config.window_rule = vec![WindowRule {
            app_id: Some(Regex::new("wide").unwrap()),
            width: Some(200),
            ..Default::default()
        }];

        let mut state = AppState::default();

        let w1 = WindowState {
            id: 1,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        };
        let w2 = WindowState {
            id: 2,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        };
        state.windows.push(w1); // Will be processed first
        state.windows.push(w2); // Will be processed second

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        reorder(&mut ctx).expect("Reorder failed");

        let actions = &ctx.socket.sent_actions;

        // Window 1 (Special):
        // X = Start (0) + Offset (0) = 0
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow {
                id: Some(1),
                x: PositionChange::SetFixed(x),
                ..
            } if *x == 0.0
        )));

        // Window 2 (Default):
        // X = Start (0) + Offset (Width1 + Gap) = 0 + 200 + 10 = 110
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow {
                id: Some(2),
                x: PositionChange::SetFixed(x),
                ..
            } if *x == 110.0
        )));
    }

    #[test]
    fn test_window_rules_right_height_stacking_mixed() {
        let temp_dir = tempdir().unwrap();
        // Scenario: Right side.
        // Window 1: Default (Height 200)
        // Window 2: Special (Height 400)
        // Window 3: Default (Height 200)
        let w1 = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let mut w2 = mock_window(2, false, true, 1, Some((1.0, 2.0)));
        w2.app_id = Some("tall".into());
        let w3 = mock_window(3, false, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w1, w2, w3]);
        let mut config = mock_config();
        config.interaction.position = SidebarPosition::Right;
        config.geometry.height = 200;
        config.geometry.gap = 10;
        config.margins.top = 0;
        config.margins.right = 0;
        config.margins.bottom = 0;
        config.window_rule = vec![WindowRule {
            app_id: Some(Regex::new("tall").unwrap()),
            height: Some(400),
            ..Default::default()
        }];
        let mut state = AppState::default();
        let w1 = WindowState {
            id: 1,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        };
        let w2 = WindowState {
            id: 2,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        };
        let w3 = WindowState {
            id: 3,
            width: 300,
            height: 200,
            is_floating: false,
            position: None,
        };
        state.windows.push(w1);
        state.windows.push(w2);
        state.windows.push(w3);
        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };
        reorder(&mut ctx).expect("Reorder failed");
        let actions = &ctx.socket.sent_actions;
        // Screen H: 1080

        // Window 1 (Default):
        // Height 200.
        // Y = ScreenH - Height - MarginTop - Offset
        // Y = 1080 - 200 - 0 - 0 = 880
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow {
                id: Some(1),
                y: PositionChange::SetFixed(y),
                ..
            } if *y == 880.0
        )));
        // Window 2 (Tall):
        // Height 400.
        // Previous Offset = Height1 + Gap = 200 + 10 = 210
        // Y = ScreenH - Height - MarginTop - Offset
        // Y = 1080 - 400 - 0 - 210 = 470
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow {
                id: Some(2),
                y: PositionChange::SetFixed(y),
                ..
            } if *y == 470.0
        )));
        // Window 3 (Default):
        // Height 200.
        // Previous Offset = 210 + Height2 + Gap = 210 + 400 + 10 = 620
        // Y = ScreenH - Height - MarginTop - Offset
        // Y = 1080 - 200 - 0 - 620 = 260
        assert!(actions.iter().any(|a| matches!(a,
            Action::MoveFloatingWindow {
                id: Some(3),
                y: PositionChange::SetFixed(y),
                ..
            } if *y == 260.0
        )));
    }
}
