//! grok-style color slots (docs/01-grok-tui-spec.md section 9).
//! Values are full RGB; quantization to 256/16-color terminals is a later step.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    pub bg_base: Color,
    pub bg_light: Color,
    pub bg_highlight: Color,
    pub text_primary: Color,
    pub text_secondary: Color,
    pub gray_dim: Color,
    pub gray: Color,
    pub gray_bright: Color,
    pub accent_user: Color,
    pub accent_assistant: Color,
    pub accent_thinking: Color,
    pub accent_tool: Color,
    pub accent_error: Color,
    pub accent_success: Color,
    pub accent_running: Color,
    pub accent_plan: Color,
    pub border: Color,
    pub prompt_border: Color,
    pub prompt_border_active: Color,
    pub code_bg: Color,
    pub diff_insert_fg: Color,
    pub diff_delete_fg: Color,
    pub warning: Color,
}

pub const DARK: Theme = Theme {
    name: "dark",
    bg_base: Color::Rgb(16, 16, 20),
    bg_light: Color::Rgb(24, 24, 30),
    bg_highlight: Color::Rgb(32, 32, 40),
    text_primary: Color::Rgb(224, 224, 232),
    text_secondary: Color::Rgb(160, 160, 172),
    gray_dim: Color::Rgb(96, 96, 108),
    gray: Color::Rgb(128, 128, 140),
    gray_bright: Color::Rgb(176, 176, 188),
    accent_user: Color::Rgb(110, 180, 255),
    accent_assistant: Color::Rgb(224, 224, 232),
    accent_thinking: Color::Rgb(180, 160, 255),
    accent_tool: Color::Rgb(255, 180, 90),
    accent_error: Color::Rgb(255, 100, 100),
    accent_success: Color::Rgb(120, 220, 140),
    accent_running: Color::Rgb(255, 220, 110),
    accent_plan: Color::Rgb(255, 160, 220),
    border: Color::Rgb(64, 64, 76),
    prompt_border: Color::Rgb(64, 64, 76),
    prompt_border_active: Color::Rgb(150, 150, 170),
    code_bg: Color::Rgb(28, 28, 36),
    diff_insert_fg: Color::Rgb(120, 220, 140),
    diff_delete_fg: Color::Rgb(255, 120, 120),
    warning: Color::Rgb(255, 200, 90),
};

pub const LIGHT: Theme = Theme {
    name: "light",
    bg_base: Color::Rgb(250, 250, 252),
    bg_light: Color::Rgb(240, 240, 244),
    bg_highlight: Color::Rgb(232, 232, 238),
    text_primary: Color::Rgb(28, 28, 34),
    text_secondary: Color::Rgb(100, 100, 112),
    gray_dim: Color::Rgb(150, 150, 160),
    gray: Color::Rgb(120, 120, 130),
    gray_bright: Color::Rgb(80, 80, 92),
    accent_user: Color::Rgb(30, 110, 220),
    accent_assistant: Color::Rgb(40, 40, 48),
    accent_thinking: Color::Rgb(120, 90, 220),
    accent_tool: Color::Rgb(200, 120, 30),
    accent_error: Color::Rgb(210, 60, 60),
    accent_success: Color::Rgb(30, 160, 70),
    accent_running: Color::Rgb(180, 140, 0),
    accent_plan: Color::Rgb(190, 80, 160),
    border: Color::Rgb(210, 210, 220),
    prompt_border: Color::Rgb(210, 210, 220),
    prompt_border_active: Color::Rgb(110, 110, 130),
    code_bg: Color::Rgb(238, 238, 242),
    diff_insert_fg: Color::Rgb(30, 160, 70),
    diff_delete_fg: Color::Rgb(210, 60, 60),
    warning: Color::Rgb(190, 130, 0),
};

pub fn theme_for(name: &str) -> Theme {
    match name {
        "light" | "day" => LIGHT,
        _ => DARK,
    }
}
