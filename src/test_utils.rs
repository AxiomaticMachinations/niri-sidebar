use crate::config::{Geometry, Margins};
use crate::{Config, NiriClient};
use anyhow::Result;
use niri_ipc::{Action, Response, Window, WindowLayout, Workspace};

#[derive(Default, Debug, Clone)]
pub struct MockNiri {
    pub windows: Vec<Window>,
    pub sent_actions: Vec<Action>,
    pub workspaces: Vec<Workspace>,
}

impl MockNiri {
    pub fn new(windows: Vec<Window>) -> Self {
        Self {
            windows,
            sent_actions: vec![],
            workspaces: vec![mock_workspace(1, "eDP-1", true, true)],
        }
    }

    pub fn with_workspaces(mut self, workspaces: Vec<Workspace>) -> Self {
        self.workspaces = workspaces;
        self
    }
}

impl NiriClient for MockNiri {
    fn get_windows(&mut self) -> Result<Vec<Window>> {
        Ok(self.windows.clone())
    }

    fn get_workspaces(&mut self) -> Result<Vec<Workspace>> {
        Ok(self.workspaces.clone())
    }

    fn get_active_window(&mut self) -> Result<Window> {
        self.windows
            .iter()
            .find(|w| w.is_focused)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("No active window in mock"))
    }

    fn send_action(&mut self, action: Action) -> Result<Response> {
        self.sent_actions.push(action);
        Ok(Response::Handled)
    }

    fn get_active_workspace(&mut self) -> Result<Workspace> {
        self.workspaces
            .iter()
            .find(|workspace| workspace.is_focused)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("No active workspace in mock"))
    }

    fn get_screen_dimensions(&mut self) -> Result<(i32, i32)> {
        Ok((1920, 1080))
    }

    fn get_workspace_output(&mut self, _workspace_id: u64) -> Result<String> {
        Ok("eDP-1".into())
    }
}

pub fn mock_workspace(id: u64, output: &str, is_active: bool, is_focused: bool) -> Workspace {
    Workspace {
        id,
        idx: id as u8,
        name: Some(format!("test-{id}")),
        output: Some(output.into()),
        is_urgent: false,
        is_active,
        is_focused,
        active_window_id: None,
    }
}

pub fn mock_window(
    id: u64,
    is_focused: bool,
    is_floating: bool,
    workspace_id: u64,
    position: Option<(f64, f64)>,
) -> Window {
    Window {
        id,
        is_focused,
        is_floating,
        workspace_id: Some(workspace_id),
        title: Some("Test Window".into()),
        app_id: Some("test".into()),
        pid: Some(123),
        is_urgent: false,
        layout: WindowLayout {
            window_size: (100, 100),
            pos_in_scrolling_layout: None,
            tile_size: (0.0, 0.0),
            tile_pos_in_workspace_view: position,
            window_offset_in_tile: (0.0, 0.0),
        },
        focus_timestamp: None,
    }
}

pub fn mock_config() -> Config {
    Config {
        geometry: Geometry {
            width: 300,
            height: 200,
            gap: 10,
        },
        margins: Margins {
            top: 50,
            right: 20,
            left: 10,
            bottom: 50,
        },
        ..Default::default()
    }
}
