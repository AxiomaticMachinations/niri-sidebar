use crate::commands::movefrom::move_to;
use crate::commands::reorder::reconcile_positions;
#[cfg(test)]
use crate::commands::reorder::reorder;
use crate::commands::togglewindow::add_to_sidebar;
use crate::config::load_config;
use crate::niri::connect;
use crate::state::{get_default_cache_dir, load_state, save_state};
use crate::window_rules::resolve_auto_add;
use crate::{Ctx, NiriClient};
use anyhow::{Result, bail};
use fslock::LockFile;
use niri_ipc::socket::Socket;
use niri_ipc::{Event, Request, Window};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

const EVENT_DEBOUNCE: Duration = Duration::from_millis(50);
const HEALTH_CHECK: Duration = Duration::from_secs(1);
const VERIFY_DELAY: Duration = Duration::from_millis(100);
const VERIFY_ATTEMPTS: usize = 3;

pub fn listen(ctx: Ctx<Socket>) -> Result<()> {
    if ctx.config.interaction.hover {
        super::hover::spawn(&ctx.config);
    }

    let (tx, rx) = mpsc::channel();
    let mut initial_socket = Some(ctx.socket);
    std::thread::spawn(move || {
        loop {
            let mut event_socket = match initial_socket.take() {
                Some(socket) => socket,
                None => match connect() {
                    Ok(socket) => socket,
                    Err(error) => {
                        eprintln!(
                            "niri-sidebar: event stream reconnect failed: {error:#}; retrying"
                        );
                        std::thread::sleep(HEALTH_CHECK);
                        continue;
                    }
                },
            };
            if let Err(error) = event_socket.send(Request::EventStream) {
                eprintln!("niri-sidebar: event stream setup failed: {error:#}; retrying");
                std::thread::sleep(HEALTH_CHECK);
                continue;
            }
            let mut read_event = event_socket.read_events();
            loop {
                match read_event() {
                    Ok(event) => {
                        if tx.send(event).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        eprintln!("niri-sidebar: event stream failed: {error:#}; reconnecting");
                        break;
                    }
                }
            }
            std::thread::sleep(HEALTH_CHECK);
        }
    });

    println!("niri-sidebar: Listening for window events...");

    loop {
        match rx.recv_timeout(HEALTH_CHECK) {
            Ok(event) => {
                let mut events = vec![event];
                loop {
                    match rx.recv_timeout(EVENT_DEBOUNCE) {
                        Ok(event) => events.push(event),
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => {
                            bail!("niri event stream disconnected")
                        }
                    }
                }
                if let Err(error) = process_event_batch(events) {
                    eprintln!("niri-sidebar: reconcile after event failed: {error:#}; retrying");
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if let Err(error) = reconcile() {
                    eprintln!("niri-sidebar: periodic reconcile failed: {error:#}; retrying");
                }
            }
            Err(RecvTimeoutError::Disconnected) => bail!("niri event stream disconnected"),
        }
    }
}

pub(crate) fn get_ctx() -> Result<(Ctx<Socket>, LockFile)> {
    let cache_dir = get_default_cache_dir()?;
    let mut lock_path = cache_dir.clone();
    lock_path.push("instance.lock");
    let mut lock_file = LockFile::open(&lock_path)?;
    lock_file.lock()?;

    let state = load_state(&cache_dir)?;
    let config = load_config();
    let ctx = Ctx {
        state,
        config,
        socket: connect()?,
        cache_dir,
    };

    Ok((ctx, lock_file))
}

fn process_event_batch(events: Vec<Event>) -> Result<()> {
    let (mut ctx, _lock) = get_ctx()?;
    let mut dirty = false;

    for event in events {
        match event {
            Event::WindowClosed { id } => dirty |= update_closed_window(&mut ctx, id)?,
            Event::WindowFocusChanged { .. } => dirty = true,
            Event::WorkspaceActivated { id, focused: true } => {
                if ctx.config.interaction.sticky {
                    process_move(&mut ctx, id)?;
                }
                dirty = true;
            }
            Event::WindowOpenedOrChanged { window } => {
                update_new_window(&mut ctx, &window)?;
                dirty = true;
            }
            _ => {}
        }
    }

    if dirty {
        reconcile_ctx(&mut ctx)?;
    }
    Ok(())
}

fn reconcile() -> Result<()> {
    let (mut ctx, _lock) = get_ctx()?;
    reconcile_ctx(&mut ctx)
}

fn reconcile_ctx<C: NiriClient>(ctx: &mut Ctx<C>) -> Result<()> {
    for attempt in 0..VERIFY_ATTEMPTS {
        if !reconcile_positions(ctx)? {
            return Ok(());
        }
        if attempt + 1 < VERIFY_ATTEMPTS {
            std::thread::sleep(VERIFY_DELAY);
        }
    }
    Ok(())
}

#[cfg(test)]
pub fn process_close<C: NiriClient>(ctx: &mut Ctx<C>, closed_id: u64) -> Result<()> {
    if update_closed_window(ctx, closed_id)? {
        reorder(ctx)?;
    }
    Ok(())
}

fn update_closed_window<C: NiriClient>(ctx: &mut Ctx<C>, closed_id: u64) -> Result<bool> {
    if let Some(index) = ctx.state.windows.iter().position(|w| w.id == closed_id) {
        println!("Sidebar window {} closed. Reordering...", closed_id);

        ctx.state.windows.remove(index);
        save_state(&ctx.state, &ctx.cache_dir)?;
        dbg!(&ctx.state);
        return Ok(true);
    }
    Ok(false)
}

pub fn process_move<C: NiriClient>(ctx: &mut Ctx<C>, ws_id: u64) -> Result<()> {
    let windows: Vec<_> = ctx.socket.get_windows()?;
    let sidebar_windows = windows
        .iter()
        .filter(|w| ctx.state.windows.iter().any(|ws| ws.id == w.id))
        .collect();
    move_to(ctx, sidebar_windows, ws_id)?;
    Ok(())
}

#[cfg(test)]
pub fn process_new_window<C: NiriClient>(ctx: &mut Ctx<C>, window: &Window) -> Result<()> {
    if update_new_window(ctx, window)? {
        reorder(ctx)?;
    }
    Ok(())
}

fn update_new_window<C: NiriClient>(ctx: &mut Ctx<C>, window: &Window) -> Result<bool> {
    // If window is removed from sidebar a WindowOpenedOrChanged event will happen
    // and this if let will catch that and remove id from vector, prevents auto_add
    // from being triggered immediately after window is removed from sidebar
    if let Some(index) = ctx
        .state
        .ignored_windows
        .iter()
        .position(|id| id == &window.id)
    {
        ctx.state.ignored_windows.remove(index);
        save_state(&ctx.state, &ctx.cache_dir)?;
        return Ok(false);
    }

    if resolve_auto_add(&ctx.config.window_rule, window)
        && !ctx.state.windows.iter().any(|w| w.id == window.id)
    {
        add_to_sidebar(ctx, window)?;
        save_state(&ctx.state, &ctx.cache_dir)?;
        return Ok(true);
    }

    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, WindowRule};
    use crate::state::{AppState, WindowState};
    use crate::test_utils::{MockNiri, mock_window};
    use niri_ipc::{Action, WorkspaceReferenceArg};
    use regex::Regex;
    use tempfile::tempdir;

    #[test]
    fn test_process_close_removes_window_and_reorders() {
        let temp_dir = tempdir().unwrap();
        unsafe {
            std::env::set_var("NIRI_SIDEBAR_TEST_DIR", temp_dir.path());
        }

        let mut state = AppState::default();
        let w1 = WindowState {
            id: 100,
            width: 500,
            height: 500,
            is_floating: false,
            position: None,
        };
        let w2 = WindowState {
            id: 200,
            width: 500,
            height: 500,
            is_floating: true,
            position: Some((1.0, 2.0)),
        };
        state.windows.push(w1);
        state.windows.push(w2);

        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let w200 = mock_window(200, false, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w100, w200]);

        let mut ctx = Ctx {
            state,
            config: Config::default(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        process_close(&mut ctx, 100).expect("Process close failed");

        // 100 removed
        assert!(!ctx.state.windows.iter().any(|w| w.id == 100));
        assert_eq!(ctx.state.windows.len(), 1);
        assert_eq!(ctx.state.windows[0].id, 200);
        // Reorder should have run (sending actions)
        assert!(!ctx.socket.sent_actions.is_empty());
    }

    #[test]
    fn test_process_close_ignores_unknown_window() {
        let temp_dir = tempdir().unwrap();
        unsafe {
            std::env::set_var("NIRI_SIDEBAR_TEST_DIR", temp_dir.path());
        }

        let mut state = AppState::default();
        let w1 = WindowState {
            id: 100,
            width: 500,
            height: 500,
            is_floating: false,
            position: None,
        };
        state.windows.push(w1);

        let mock = MockNiri::new(vec![]);

        let mut ctx = Ctx {
            state,
            config: Config::default(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        process_close(&mut ctx, 999).expect("Process close failed");

        // State should still have Window 100
        assert_eq!(ctx.state.windows.len(), 1);
        assert_eq!(ctx.state.windows[0].id, 100);

        // No reorder actions should have been sent
        assert!(ctx.socket.sent_actions.is_empty());
    }

    #[test]
    fn test_process_move_consolidates_tracked_windows_from_all_workspaces() {
        let temp_dir = tempdir().unwrap();
        unsafe {
            std::env::set_var("NIRI_SIDEBAR_TEST_DIR", temp_dir.path());
        }

        let mut state = AppState::default();
        let w1 = WindowState {
            id: 10,
            width: 100,
            height: 200,
            is_floating: true,
            position: Some((1.0, 2.0)),
        };
        let w2 = WindowState {
            id: 20,
            width: 100,
            height: 200,
            is_floating: true,
            position: Some((1.0, 2.0)),
        };
        state.windows.push(w1);
        state.windows.push(w2);

        // Window 10: Tracked, on WS 1
        let w10 = mock_window(10, true, false, 1, Some((1.0, 2.0)));
        // Window 20: Tracked, on WS 2
        let w20 = mock_window(20, true, false, 2, Some((1.0, 2.0)));
        // Window 30: Untracked, on WS 1
        let w30 = mock_window(30, true, false, 1, Some((1.0, 2.0)));

        let mock = MockNiri::new(vec![w10, w20, w30]);

        let mut ctx = Ctx {
            state,
            config: Config::default(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        let target_ws = 99;

        process_move(&mut ctx, target_ws).expect("process_move failed");
        let actions = &ctx.socket.sent_actions;

        // Should have 2 actions (for ID 10 and 20)
        assert_eq!(actions.len(), 2);

        let check_action = |act: &Action, expected_id: u64| {
            if let Action::MoveWindowToWorkspace {
                window_id,
                reference,
                ..
            } = act
            {
                assert_eq!(*window_id, Some(expected_id));
                match reference {
                    WorkspaceReferenceArg::Id(id) => assert_eq!(*id, target_ws),
                    _ => panic!("Wrong target workspace"),
                }
            } else {
                panic!("Wrong action type");
            }
        };

        check_action(&actions[0], 10);
        check_action(&actions[1], 20);
    }

    #[test]
    fn test_process_new_window_adds_when_autoadd_true() {
        let temp_dir = tempdir().unwrap();
        unsafe {
            std::env::set_var("NIRI_SIDEBAR_TEST_DIR", temp_dir.path());
        }

        let state = AppState::default();

        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w100]);

        let config = Config {
            window_rule: vec![WindowRule {
                app_id: Some(Regex::new(r"test").unwrap()),
                auto_add: true,
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        process_new_window(&mut ctx, &w100).expect("Process new window failed");

        // 100 added
        assert!(ctx.state.windows.iter().any(|w| w.id == 100));
        assert_eq!(ctx.state.windows.len(), 1);
        assert_eq!(ctx.state.windows[0].id, 100);
        // Reorder should have run (sending actions)
        assert!(!ctx.socket.sent_actions.is_empty());
    }

    #[test]
    fn test_process_new_window_ignores_when_autoadd_false() {
        let temp_dir = tempdir().unwrap();
        unsafe {
            std::env::set_var("NIRI_SIDEBAR_TEST_DIR", temp_dir.path());
        }

        let state = AppState::default();

        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w100]);

        let config = Config {
            window_rule: vec![WindowRule {
                app_id: Some(Regex::new(r"test").unwrap()),
                auto_add: false,
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        process_new_window(&mut ctx, &w100).expect("Process new window failed");

        // 100 ignored
        assert!(!ctx.state.windows.iter().any(|w| w.id == 100));
        assert_eq!(ctx.state.windows.len(), 0);
        // Reorder should not have run
        assert!(ctx.socket.sent_actions.is_empty());
    }

    #[test]
    fn test_process_new_window_ignores_when_no_rule() {
        let temp_dir = tempdir().unwrap();
        unsafe {
            std::env::set_var("NIRI_SIDEBAR_TEST_DIR", temp_dir.path());
        }

        let state = AppState::default();

        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w100]);

        let mut ctx = Ctx {
            state,
            config: Config::default(),
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        process_new_window(&mut ctx, &w100).expect("Process new window failed");

        // 100 ignored
        assert!(!ctx.state.windows.iter().any(|w| w.id == 100));
        assert_eq!(ctx.state.windows.len(), 0);
        // Reorder should not have run
        assert!(ctx.socket.sent_actions.is_empty());
    }

    #[test]
    fn test_process_new_window_ignores_after_removed_from_sidebar() {
        let temp_dir = tempdir().unwrap();
        unsafe {
            std::env::set_var("NIRI_SIDEBAR_TEST_DIR", temp_dir.path());
        }

        let mut state = AppState::default();
        state.ignored_windows.push(100);

        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        let mock = MockNiri::new(vec![w100]);

        let config = Config {
            window_rule: vec![WindowRule {
                app_id: Some(Regex::new(r"test").unwrap()),
                auto_add: true,
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut ctx = Ctx {
            state,
            config,
            socket: mock,
            cache_dir: temp_dir.path().to_path_buf(),
        };

        let w100 = mock_window(100, true, true, 1, Some((1.0, 2.0)));
        process_new_window(&mut ctx, &w100).expect("Process new window failed");

        // 100 ignored
        assert!(!ctx.state.windows.iter().any(|w| w.id == 100));
        assert_eq!(ctx.state.windows.len(), 0);
        // Reorder should not have run
        assert!(ctx.socket.sent_actions.is_empty());
    }
}
