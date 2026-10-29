use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

pub const DEFAULT_CONFIG_STR: &str = include_str!("../default_config.toml");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SidebarPosition {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackDirection {
    Auto,
    TopToBottom,
    BottomToTop,
    LeftToRight,
    RightToLeft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HoverMode {
    /// Hovering the screen edge shows the whole sidebar
    #[default]
    All,
    /// Hovering a window's edge strip reveals only that window
    Window,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub geometry: Geometry,
    pub margins: Margins,
    pub interaction: Interaction,
    #[serde(default)]
    pub window_rule: Vec<WindowRule>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Geometry {
    pub width: i32,
    pub height: i32,
    pub gap: i32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Margins {
    #[serde(default = "default_margin")]
    pub top: i32,
    #[serde(default = "default_margin")]
    pub right: i32,
    #[serde(default = "default_margin")]
    pub left: i32,
    #[serde(default = "default_margin")]
    pub bottom: i32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Interaction {
    pub peek: i32,
    pub focus_peek: Option<i32>,
    #[serde(default = "default_position")]
    pub position: SidebarPosition,
    #[serde(default = "default_stack_direction")]
    pub stack_direction: StackDirection,
    #[serde(default = "default_sticky")]
    pub sticky: bool,
    #[serde(default)]
    pub hover: bool,
    #[serde(default)]
    pub hover_mode: HoverMode,
    pub hover_zone_width: Option<i32>,
}

impl Interaction {
    pub fn get_focus_peek(&self) -> i32 {
        self.focus_peek.unwrap_or(self.peek)
    }

    pub fn get_stack_direction(&self) -> StackDirection {
        match (self.position, self.stack_direction) {
            (_, StackDirection::Auto) => default_stack_direction_for_position(self.position),
            (
                SidebarPosition::Left | SidebarPosition::Right,
                StackDirection::TopToBottom | StackDirection::BottomToTop,
            ) => self.stack_direction,
            (
                SidebarPosition::Top | SidebarPosition::Bottom,
                StackDirection::LeftToRight | StackDirection::RightToLeft,
            ) => self.stack_direction,
            _ => {
                eprintln!(
                    "interaction.stack_direction {:?} is not compatible with {:?}; using the default for that position.",
                    self.stack_direction, self.position
                );
                default_stack_direction_for_position(self.position)
            }
        }
    }
}

fn default_sticky() -> bool {
    false
}

fn default_stack_direction() -> StackDirection {
    StackDirection::Auto
}

fn default_position() -> SidebarPosition {
    SidebarPosition::Right
}

fn default_stack_direction_for_position(position: SidebarPosition) -> StackDirection {
    match position {
        SidebarPosition::Left | SidebarPosition::Right => StackDirection::BottomToTop,
        SidebarPosition::Top | SidebarPosition::Bottom => StackDirection::LeftToRight,
    }
}

fn default_margin() -> i32 {
    0
}

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct WindowRule {
    #[serde(default, with = "serde_regex")]
    pub app_id: Option<Regex>,
    #[serde(default, with = "serde_regex")]
    pub title: Option<Regex>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub peek: Option<i32>,
    pub focus_peek: Option<i32>,
    #[serde(default)]
    pub auto_add: bool,
}

impl Default for Config {
    fn default() -> Self {
        toml::from_str(DEFAULT_CONFIG_STR).expect("Default config file is invalid TOML")
    }
}

pub fn get_config_dir() -> Result<PathBuf> {
    let mut path = dirs::config_dir().context("Could not find config directory")?;
    path.push("niri-sidebar");
    Ok(path)
}

pub fn load_config() -> Config {
    let Ok(mut path) = get_config_dir() else {
        return Config::default();
    };
    path.push("config.toml");

    if path.exists() {
        if let Ok(content) = fs::read_to_string(&path) {
            match toml::from_str(&content) {
                Ok(cfg) => return cfg,
                Err(e) => eprintln!("Error parsing config.toml: {}. Using defaults.", e),
            }
        }
    }
    Config::default()
}

pub fn init_config() -> Result<()> {
    let mut path = get_config_dir()?;

    if !path.exists() {
        fs::create_dir_all(&path)?;
        println!("Created directory: {:?}", path);
    }

    path.push("config.toml");

    if path.exists() {
        anyhow::bail!("Config file already exists at {:?}", path);
    }

    fs::write(&path, DEFAULT_CONFIG_STR)?;
    println!("Default config written to {:?}", path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_interaction_stack_direction_defaults_to_auto() {
        let config: Config = toml::from_str(
            r#"
                [geometry]
                width = 400
                height = 335
                gap = 10

                [margins]
                top = 50
                right = 10
                left = 10
                bottom = 10

                [interaction]
                position = "right"
                peek = 10
                focus_peek = 50
                sticky = false
            "#,
        )
        .unwrap();

        assert_eq!(config.interaction.stack_direction, StackDirection::Auto);
        assert_eq!(
            config.interaction.get_stack_direction(),
            StackDirection::BottomToTop
        );
    }

    #[test]
    fn test_interaction_stack_direction_can_be_top_to_bottom() {
        let config: Config = toml::from_str(
            r#"
                [geometry]
                width = 400
                height = 335
                gap = 10

                [margins]
                top = 50
                right = 10
                left = 10
                bottom = 10

                [interaction]
                position = "right"
                peek = 10
                focus_peek = 50
                stack_direction = "top_to_bottom"
                sticky = false
            "#,
        )
        .unwrap();

        assert_eq!(
            config.interaction.get_stack_direction(),
            StackDirection::TopToBottom
        );
    }
}
