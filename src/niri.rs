use anyhow::{Context, Result, bail};
use niri_ipc::Action;
use niri_ipc::{Request, Response, socket::Socket};
pub use niri_ipc::{Window, Workspace};

pub trait NiriClient {
    fn get_windows(&mut self) -> Result<Vec<Window>>;
    fn get_workspaces(&mut self) -> Result<Vec<Workspace>>;
    fn get_active_window(&mut self) -> Result<Window>;
    fn get_active_workspace(&mut self) -> Result<Workspace>;
    fn get_screen_dimensions(&mut self) -> Result<(i32, i32)>;
    fn get_workspace_output(&mut self, workspace_id: u64) -> Result<String>;
    /// Logical size of the output a workspace is on.
    fn get_workspace_dimensions(&mut self, _workspace_id: u64) -> Result<(i32, i32)> {
        self.get_screen_dimensions()
    }
    fn send_action(&mut self, action: Action) -> Result<Response>;
}

pub fn connect() -> Result<Socket> {
    Socket::connect().context("Failed to connect to Niri socket")
}

impl NiriClient for Socket {
    fn get_windows(&mut self) -> Result<Vec<Window>> {
        match self.send(Request::Windows)? {
            Ok(Response::Windows(windows)) => Ok(windows),
            _ => bail!("Unexpected response from Niri when fetching windows"),
        }
    }

    fn get_workspaces(&mut self) -> Result<Vec<Workspace>> {
        match self.send(Request::Workspaces)? {
            Ok(Response::Workspaces(workspaces)) => Ok(workspaces),
            _ => bail!("Unexpected response from Niri when fetching workspaces"),
        }
    }

    fn get_active_window(&mut self) -> Result<Window> {
        match self.send(Request::FocusedWindow)? {
            Ok(Response::FocusedWindow(Some(window))) => Ok(window),
            Ok(Response::FocusedWindow(None)) => bail!("No window focused"),
            _ => bail!("Unexpected response from Niri when fetching windows"),
        }
    }

    fn get_active_workspace(&mut self) -> Result<Workspace> {
        self.get_workspaces()?
            .into_iter()
            .find(|w| w.is_focused)
            .context("No active workspace found")
    }

    fn get_screen_dimensions(&mut self) -> Result<(i32, i32)> {
        let id = self.get_active_workspace()?.id;
        self.get_workspace_dimensions(id)
    }

    fn get_workspace_dimensions(&mut self, workspace_id: u64) -> Result<(i32, i32)> {
        let target_output_name = self.get_workspace_output(workspace_id)?;

        match self.send(Request::Outputs)? {
            Ok(Response::Outputs(outputs)) => {
                let output = outputs
                    .values()
                    .find(|o| o.name == target_output_name)
                    .context("Output not found")?;

                // Return the logical size
                let logical = output
                    .logical
                    .as_ref()
                    .context("Output has no logical size")?;
                Ok((
                    logical.width.try_into().unwrap_or(1920),
                    logical.height.try_into().unwrap_or(1080),
                ))
            }
            _ => bail!("Unexpected response from Niri when fetching outputs"),
        }
    }

    fn get_workspace_output(&mut self, workspace_id: u64) -> Result<String> {
        self.get_workspaces()?
            .into_iter()
            .find(|w| w.id == workspace_id)
            .and_then(|w| w.output)
            .context("Workspace is not on an output")
    }

    fn send_action(&mut self, action: Action) -> Result<Response> {
        self.send(Request::Action(action))?
            .map_err(|e| anyhow::anyhow!(e))
    }
}

/// The workspace the sidebar lives on: the focused one if it holds sidebar windows,
/// otherwise wherever the sidebar windows are (e.g. focus is on another monitor).
pub fn sidebar_workspace(windows: &[Window], sidebar_ids: &[u64], focused_ws: u64) -> u64 {
    let mut sidebar = sidebar_ids
        .iter()
        .filter_map(|id| windows.iter().find(|w| w.id == *id && w.is_floating))
        .filter_map(|w| w.workspace_id);
    let first = sidebar.next();
    if first == Some(focused_ws) || sidebar.any(|ws| ws == focused_ws) {
        return focused_ws;
    }
    first.unwrap_or(focused_ws)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_utils::mock_window;

    #[test]
    fn sidebar_workspace_follows_sidebar_windows_when_focus_is_elsewhere() {
        let windows = vec![
            mock_window(1, false, true, 1, None),
            mock_window(2, true, false, 2, None),
        ];
        assert_eq!(sidebar_workspace(&windows, &[1], 2), 1);
    }

    #[test]
    fn sidebar_workspace_prefers_focused_workspace_when_it_has_sidebar_windows() {
        let windows = vec![
            mock_window(1, false, true, 1, None),
            mock_window(2, false, true, 2, None),
        ];
        assert_eq!(sidebar_workspace(&windows, &[1, 2], 2), 2);
    }

    #[test]
    fn sidebar_workspace_falls_back_to_focused_without_sidebar_windows() {
        assert_eq!(sidebar_workspace(&[], &[], 3), 3);
    }
}
