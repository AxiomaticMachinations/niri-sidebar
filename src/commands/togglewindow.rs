use crate::Ctx;
use crate::commands::reorder;
use crate::niri::NiriClient;
use crate::state::{WindowState, save_state};
use crate::window_rules::resolve_window_size;
use anyhow::{Context, Result};
use niri_ipc::{Action, SizeChange, Window};

pub fn toggle_window<C: NiriClient>(ctx: &mut Ctx<C>) -> Result<()> {
    let focused = ctx.socket.get_active_window()?;

    let is_tracked = ctx.state.windows.iter().any(|w| w.id == focused.id);

    if is_tracked {
        remove_from_sidebar(ctx, &focused)?;
    } else {
        add_to_sidebar(ctx, &focused)?;
    }

    save_state(&ctx.state, &ctx.cache_dir)?;
    reorder(ctx)?;

    Ok(())
}

enum Height {
    Fixed(i32),
    Auto,
}

fn send_size<C: NiriClient>(ctx: &mut Ctx<C>, id: u64, width: i32, height: &Height) {
    let _ = ctx.socket.send_action(Action::SetWindowWidth {
        change: SizeChange::SetFixed(width),
        id: Some(id),
    });
    let _ = match height {
        Height::Fixed(h) => ctx.socket.send_action(Action::SetWindowHeight {
            change: SizeChange::SetFixed(*h),
            id: Some(id),
        }),
        Height::Auto => ctx
            .socket
            .send_action(Action::ResetWindowHeight { id: Some(id) }),
    };
}

/// Sets a window's size. niri can drop size requests sent right after a window
/// changes between floating and tiled, so when `verify` is set the size is re-sent
/// until the window reports it (or stops changing, e.g. at an app's minimum size).
fn apply_size<C: NiriClient>(ctx: &mut Ctx<C>, id: u64, width: i32, height: Height, verify: bool) {
    send_size(ctx, id, width, &height);
    if !verify {
        return;
    }

    let mut last = None;
    let mut stable = 0;
    for attempt in 0..8 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        let Some(size) = ctx
            .socket
            .get_windows()
            .ok()
            .and_then(|ws| ws.into_iter().find(|w| w.id == id))
            .map(|w| w.layout.window_size)
        else {
            return;
        };
        let width_ok = size.0 == width;
        let height_ok = match height {
            Height::Fixed(h) => size.1 == h,
            Height::Auto => attempt >= 1,
        };
        if width_ok && height_ok {
            return;
        }
        stable = if last == Some(size) { stable + 1 } else { 0 };
        last = Some(size);
        if width_ok && stable >= 2 {
            return;
        }
        send_size(ctx, id, width, &height);
    }
}

pub fn add_to_sidebar<C: NiriClient>(ctx: &mut Ctx<C>, window: &Window) -> Result<()> {
    let (width, height) = window.layout.window_size;
    let w_state = WindowState {
        id: window.id,
        width,
        height,
        is_floating: window.is_floating,
        position: window.layout.tile_pos_in_workspace_view,
    };
    let insert_index = if ctx.state.is_flipped {
        0
    } else {
        ctx.state.windows.len()
    };
    ctx.state.windows.insert(insert_index, w_state);

    if !window.is_floating {
        let _ = ctx.socket.send_action(Action::ToggleWindowFloating {
            id: Some(window.id),
        });
    }

    let (target_width, target_height) = resolve_window_size(
        &ctx.config.window_rule,
        window,
        ctx.config.geometry.width,
        ctx.config.geometry.height,
    );

    apply_size(
        ctx,
        window.id,
        target_width,
        Height::Fixed(target_height),
        !window.is_floating,
    );

    Ok(())
}

fn remove_from_sidebar<C: NiriClient>(ctx: &mut Ctx<C>, window: &Window) -> Result<()> {
    let index = ctx
        .state
        .windows
        .iter()
        .position(|w| w.id == window.id)
        .context("Window was not found in sidebar state")?;

    let w_state = ctx.state.windows.remove(index);
    ctx.state.ignored_windows.push(w_state.id);

    if window.is_floating && !w_state.is_floating {
        let _ = ctx.socket.send_action(Action::ToggleWindowFloating {
            id: Some(window.id),
        });
        // A tiled window's height is automatic (it fills the column), not a fixed size.
        apply_size(ctx, window.id, w_state.width, Height::Auto, true);
    } else {
        apply_size(
            ctx,
            window.id,
            w_state.width,
            Height::Fixed(w_state.height),
            false,
        );
    }

    if let Some((x, y)) = w_state.position
        && window.is_floating
    {
        let _ = ctx.socket.send_action(Action::MoveFloatingWindow {
            id: Some(window.id),
            x: niri_ipc::PositionChange::SetFixed(x),
            y: niri_ipc::PositionChange::SetFixed(y),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use niri_ipc::PositionChange;
    use tempfile::tempdir;

    use super::*;
    use crate::config::{Config, StackDirection};
    use crate::state::AppState;
    use crate::test_utils::{MockNiri, mock_config, mock_window};

    #[test]
    fn test_add_to_sidebar_tiled() {
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, false, 1, None);
        let mock = MockNiri::new(vec![win]);

        let config = mock_config();

        let mut ctx = Ctx {
            state: AppState::default(),
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        toggle_window(&mut ctx).expect("Command failed");

        // Window 100 should be in the sidebar list with original size (100x100)
        assert_eq!(ctx.state.windows.len(), 1);
        let w_state = &ctx.state.windows[0];
        assert_eq!(w_state.id, 100);
        assert_eq!(w_state.width, 100);
        assert_eq!(w_state.height, 100);
        assert!(!w_state.is_floating);
        assert_eq!(w_state.position, None);

        let actions = &ctx.socket.sent_actions;

        // Should toggle floating
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::ToggleWindowFloating { id: Some(100) }))
        );

        // Should set width to 300 (config width)
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SetWindowWidth {
                change: SizeChange::SetFixed(300),
                id: Some(100)
            }
        )));
    }

    #[test]
    fn test_add_to_sidebar_floating() {
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![win]);

        let config = mock_config();

        let mut ctx = Ctx {
            state: AppState::default(),
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        toggle_window(&mut ctx).expect("Command failed");

        // Window 100 should be in the sidebar list with original size (100x100)
        assert_eq!(ctx.state.windows.len(), 1);
        let w_state = &ctx.state.windows[0];
        assert_eq!(w_state.id, 100);
        assert_eq!(w_state.width, 100);
        assert_eq!(w_state.height, 100);
        assert!(w_state.is_floating);
        assert_eq!(w_state.position, Some((1.0, 2.0)));

        let actions = &ctx.socket.sent_actions;

        // Should not toggle floating
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::ToggleWindowFloating { id: Some(100) }))
        );

        // Should set width to 300 (config width)
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SetWindowWidth {
                change: SizeChange::SetFixed(300),
                id: Some(100)
            }
        )));
    }

    #[test]
    fn test_add_to_sidebar_with_window_rule() {
        let temp_dir = tempdir().unwrap();
        // Window with specific app_id to match rule
        let mut win = mock_window(100, true, false, 1, Some((1.0, 2.0)));
        win.app_id = Some("special".into());

        let mock = MockNiri::new(vec![win]);
        let mut config = mock_config();

        use crate::config::WindowRule;
        use regex::Regex;
        config.window_rule = vec![WindowRule {
            app_id: Some(Regex::new("special").unwrap()),
            width: Some(500),
            height: Some(600),
            ..Default::default()
        }];
        let mut ctx = Ctx {
            state: AppState::default(),
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };
        toggle_window(&mut ctx).expect("Command failed");

        assert_eq!(ctx.state.windows.len(), 1);

        let actions = &ctx.socket.sent_actions;
        // Should set width to 500 (Rule width), not 300 (Config default)
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SetWindowWidth {
                change: SizeChange::SetFixed(500),
                id: Some(100)
            }
        )));

        // Should set height to 600 (Rule height)
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SetWindowHeight {
                change: SizeChange::SetFixed(600),
                id: Some(100)
            }
        )));
    }

    #[test]
    fn test_add_to_sidebar_top_to_bottom_flipped_places_new_window_at_bottom() {
        let temp_dir = tempdir().unwrap();
        let existing = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let new_window = mock_window(2, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![existing, new_window]);

        let mut config = mock_config();
        config.interaction.stack_direction = StackDirection::TopToBottom;

        let mut state = AppState {
            is_flipped: true,
            ..Default::default()
        };
        state.windows.push(WindowState {
            id: 1,
            width: 300,
            height: 200,
            is_floating: true,
            position: Some((1.0, 2.0)),
        });

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        toggle_window(&mut ctx).expect("Command failed");

        assert_eq!(
            ctx.state.windows.iter().map(|w| w.id).collect::<Vec<_>>(),
            vec![2, 1]
        );

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
    fn test_add_to_sidebar_bottom_to_top_flipped_places_new_window_at_top() {
        let temp_dir = tempdir().unwrap();
        let existing = mock_window(1, false, true, 1, Some((1.0, 2.0)));
        let new_window = mock_window(2, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![existing, new_window]);

        let mut config = mock_config();
        config.interaction.stack_direction = StackDirection::BottomToTop;

        let mut state = AppState {
            is_flipped: true,
            ..Default::default()
        };
        state.windows.push(WindowState {
            id: 1,
            width: 300,
            height: 200,
            is_floating: true,
            position: Some((1.0, 2.0)),
        });

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        toggle_window(&mut ctx).expect("Command failed");

        assert_eq!(
            ctx.state.windows.iter().map(|w| w.id).collect::<Vec<_>>(),
            vec![2, 1]
        );

        let actions = &ctx.socket.sent_actions;
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::MoveFloatingWindow {
                id: Some(1),
                x: PositionChange::SetFixed(1600.0),
                y: PositionChange::SetFixed(830.0)
            }
        )));
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::MoveFloatingWindow {
                id: Some(2),
                x: PositionChange::SetFixed(1600.0),
                y: PositionChange::SetFixed(620.0)
            }
        )));
    }

    #[test]
    fn test_remove_from_sidebar_floating_restore_pos() {
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![win]);

        let mut state = AppState::default();
        let w1 = WindowState {
            id: 100,
            width: 1000,
            height: 800,
            is_floating: true,
            position: Some((1.0, 2.0)),
        };
        state.windows.push(w1);

        let mut ctx = Ctx {
            state,
            config: Config::default(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        toggle_window(&mut ctx).expect("Command failed");

        // Should be empty now
        assert!(ctx.state.windows.is_empty());

        // Should be added to ignore list
        assert!(ctx.state.ignored_windows[0] == 100);

        // Should restore original size and position
        let actions = &ctx.socket.sent_actions;

        // Should restore width to 1000
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SetWindowWidth {
                change: SizeChange::SetFixed(1000),
                id: Some(100)
            }
        )));

        // Should restore height to 800
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SetWindowHeight {
                change: SizeChange::SetFixed(800),
                id: Some(100)
            }
        )));

        // Should restore original position
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::MoveFloatingWindow {
                id: Some(100),
                x: PositionChange::SetFixed(1.0),
                y: PositionChange::SetFixed(2.0)
            }
        )));
    }

    #[test]
    fn test_remove_from_sidebar_tiled() {
        let temp_dir = tempdir().unwrap();
        let win = mock_window(100, true, false, 1, None);
        let mock = MockNiri::new(vec![win]);

        let mut state = AppState::default();
        let w1 = WindowState {
            id: 100,
            width: 1000,
            height: 800,
            is_floating: false,
            position: None,
        };
        state.windows.push(w1);

        let mut ctx = Ctx {
            state,
            config: Default::default(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        toggle_window(&mut ctx).expect("Command failed");

        // Should be empty now
        assert!(ctx.state.windows.is_empty());

        // Should be added to ignore list
        assert!(ctx.state.ignored_windows[0] == 100);

        // Should restore original size
        let actions = &ctx.socket.sent_actions;

        // Should only contain 2 actions
        assert_eq!(actions.len(), 2);

        // Should restore width to 1000
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SetWindowWidth {
                change: SizeChange::SetFixed(1000),
                id: Some(100)
            }
        )));

        // Should restore height to 800
        assert!(actions.iter().any(|a| matches!(
            a,
            Action::SetWindowHeight {
                change: SizeChange::SetFixed(800),
                id: Some(100)
            }
        )));
    }
}
