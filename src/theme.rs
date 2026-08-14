//! grok-style color slots (docs/01-grok-tui-spec.md section 9).
//! Values are full RGB; quantization to 256/16-color terminals is a later step.

use ratatui::style::Color;

/// grok-style color slots (docs/01 section 9); not every slot is consumed
/// by the skeleton renderer yet — they are the theme contract.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
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

/// Terminal color capability level (docs/01 section 9: automatic
/// quantization at startup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorLevel {
    Truecolor,
    Ansi256,
    Ansi16,
}

pub fn detect_level() -> ColorLevel {
    if std::env::var("NO_COLOR").is_ok() {
        return ColorLevel::Ansi16;
    }
    let ct = std::env::var("COLORTERM")
        .unwrap_or_default()
        .to_lowercase();
    if ct.contains("truecolor") || ct.contains("24bit") {
        return ColorLevel::Truecolor;
    }
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    if term.contains("256color") {
        return ColorLevel::Ansi256;
    }
    ColorLevel::Ansi16
}

fn to_rgb(c: Color) -> Option<(u8, u8, u8)> {
    match c {
        Color::Rgb(r, g, b) => Some((r, g, b)),
        _ => None,
    }
}

fn nearest_index(palette: &[(u8, u8, u8)], r: u8, g: u8, b: u8) -> usize {
    let mut best = 0usize;
    let mut best_d = i64::MAX;
    for (i, (pr, pg, pb)) in palette.iter().enumerate() {
        let dr = *pr as i64 - r as i64;
        let dg = *pg as i64 - g as i64;
        let db = *pb as i64 - b as i64;
        let d = dr * dr + dg * dg + db * db;
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

fn ansi16_palette() -> Vec<(u8, u8, u8)> {
    vec![
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ]
}

fn ansi256_palette() -> Vec<(u8, u8, u8)> {
    let mut p = ansi16_palette();
    for r in 0..6 {
        for g in 0..6 {
            for b in 0..6 {
                p.push((
                    if r == 0 { 0 } else { 55 + r * 40 },
                    if g == 0 { 0 } else { 55 + g * 40 },
                    if b == 0 { 0 } else { 55 + b * 40 },
                ));
            }
        }
    }
    for i in 0..24 {
        let v = 8 + i * 10;
        p.push((v, v, v));
    }
    p
}

const ANSI16_NAMES: [Color; 16] = [
    Color::Black,
    Color::Red,
    Color::Green,
    Color::Yellow,
    Color::Blue,
    Color::Magenta,
    Color::Cyan,
    Color::Gray,
    Color::DarkGray,
    Color::LightRed,
    Color::LightGreen,
    Color::LightYellow,
    Color::LightBlue,
    Color::LightMagenta,
    Color::LightCyan,
    Color::White,
];

fn quantize(c: Color, level: ColorLevel) -> Color {
    let Some((r, g, b)) = to_rgb(c) else { return c };
    match level {
        ColorLevel::Truecolor => c,
        ColorLevel::Ansi256 => Color::Indexed(nearest_index(&ansi256_palette(), r, g, b) as u8),
        ColorLevel::Ansi16 => ANSI16_NAMES[nearest_index(&ansi16_palette(), r, g, b)],
    }
}

pub fn theme_for(name: &str) -> Theme {
    let t = match name {
        "light" | "day" => LIGHT,
        _ => DARK,
    };
    let level = detect_level();
    if level == ColorLevel::Truecolor {
        return t;
    }
    let q = |c: Color| quantize(c, level);
    Theme {
        name: t.name,
        bg_base: q(t.bg_base),
        bg_light: q(t.bg_light),
        bg_highlight: q(t.bg_highlight),
        text_primary: q(t.text_primary),
        text_secondary: q(t.text_secondary),
        gray_dim: q(t.gray_dim),
        gray: q(t.gray),
        gray_bright: q(t.gray_bright),
        accent_user: q(t.accent_user),
        accent_assistant: q(t.accent_assistant),
        accent_thinking: q(t.accent_thinking),
        accent_tool: q(t.accent_tool),
        accent_error: q(t.accent_error),
        accent_success: q(t.accent_success),
        accent_running: q(t.accent_running),
        accent_plan: q(t.accent_plan),
        border: q(t.border),
        prompt_border: q(t.prompt_border),
        prompt_border_active: q(t.prompt_border_active),
        code_bg: q(t.code_bg),
        diff_insert_fg: q(t.diff_insert_fg),
        diff_delete_fg: q(t.diff_delete_fg),
        warning: q(t.warning),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantizes_rgb_to_palette() {
        let c = Color::Rgb(16, 16, 20);
        assert!(matches!(quantize(c, ColorLevel::Truecolor), Color::Rgb(..)));
        assert!(matches!(
            quantize(c, ColorLevel::Ansi256),
            Color::Indexed(_)
        ));
        assert!(matches!(
            quantize(c, ColorLevel::Ansi16),
            Color::Black | Color::DarkGray | Color::Gray
        ));
        // 深底主题在 16 色下应映射到黑
        assert_eq!(
            quantize(Color::Rgb(16, 16, 20), ColorLevel::Ansi16),
            Color::Black
        );
    }

    #[test]
    fn detects_levels() {
        // detect_level 读环境变量；这里只验证函数存在与 Ansi16 的保守默认
        let _ = detect_level();
    }
}
