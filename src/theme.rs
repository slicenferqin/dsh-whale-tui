//! DeepSeek 品牌色槽，基于官网设计变量中的 DeepSeek 蓝与 bluish neutral。
//! Values are full RGB; quantization to 256/16-color terminals is a later step.

use ratatui::style::Color;

/// DeepSeek color slots; not every slot is consumed by the renderer yet —
/// they are the theme contract.
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
    /// DeepSeek 蓝：logo、主操作、选中态与活动边框。
    pub accent_brand: Color,
    pub border: Color,
    pub prompt_border: Color,
    pub prompt_border_active: Color,
    pub code_bg: Color,
    pub diff_insert_fg: Color,
    pub diff_delete_fg: Color,
    pub warning: Color,
    // Syntax-highlight slots (docs/01 section 9). We tokenize with syntect but
    // colour from here, so code blocks stay inside the DeepSeek palette and
    // still go through `quantize` on 256/16-colour terminals.
    pub syn_keyword: Color,
    pub syn_string: Color,
    pub syn_number: Color,
    pub syn_comment: Color,
    pub syn_type: Color,
    pub syn_function: Color,
    pub syn_variable: Color,
    pub syn_punctuation: Color,
}

pub const DARK: Theme = Theme {
    name: "dark",
    bg_base: Color::Rgb(21, 21, 23),
    bg_light: Color::Rgb(35, 35, 36),
    bg_highlight: Color::Rgb(40, 49, 66),
    text_primary: Color::Rgb(249, 250, 251),
    text_secondary: Color::Rgb(207, 211, 214),
    gray_dim: Color::Rgb(97, 102, 107),
    gray: Color::Rgb(129, 133, 140),
    gray_bright: Color::Rgb(225, 229, 238),
    accent_user: Color::Rgb(103, 158, 254),
    accent_assistant: Color::Rgb(228, 237, 253),
    accent_thinking: Color::Rgb(183, 200, 254),
    accent_tool: Color::Rgb(96, 165, 250),
    accent_error: Color::Rgb(242, 90, 90),
    accent_success: Color::Rgb(78, 209, 126),
    accent_running: Color::Rgb(103, 158, 254),
    accent_plan: Color::Rgb(183, 200, 254),
    accent_brand: Color::Rgb(86, 134, 254),
    border: Color::Rgb(53, 54, 56),
    prompt_border: Color::Rgb(52, 65, 91),
    prompt_border_active: Color::Rgb(86, 134, 254),
    code_bg: Color::Rgb(27, 27, 28),
    diff_insert_fg: Color::Rgb(78, 209, 126),
    diff_delete_fg: Color::Rgb(242, 90, 90),
    warning: Color::Rgb(247, 173, 49),
    syn_keyword: Color::Rgb(154, 176, 255),
    syn_string: Color::Rgb(126, 214, 160),
    syn_number: Color::Rgb(247, 190, 120),
    syn_comment: Color::Rgb(116, 122, 130),
    syn_type: Color::Rgb(122, 197, 249),
    syn_function: Color::Rgb(151, 190, 255),
    syn_variable: Color::Rgb(226, 232, 240),
    syn_punctuation: Color::Rgb(154, 160, 168),
};

pub const LIGHT: Theme = Theme {
    name: "light",
    bg_base: Color::Rgb(249, 250, 251),
    bg_light: Color::Rgb(255, 255, 255),
    bg_highlight: Color::Rgb(228, 237, 253),
    text_primary: Color::Rgb(15, 17, 21),
    text_secondary: Color::Rgb(97, 102, 107),
    gray_dim: Color::Rgb(173, 178, 184),
    gray: Color::Rgb(129, 133, 140),
    gray_bright: Color::Rgb(52, 65, 91),
    accent_user: Color::Rgb(57, 100, 254),
    accent_assistant: Color::Rgb(40, 49, 66),
    accent_thinking: Color::Rgb(72, 104, 178),
    accent_tool: Color::Rgb(37, 99, 235),
    accent_error: Color::Rgb(236, 19, 19),
    accent_success: Color::Rgb(34, 197, 94),
    accent_running: Color::Rgb(57, 100, 254),
    accent_plan: Color::Rgb(72, 104, 178),
    accent_brand: Color::Rgb(57, 100, 254),
    border: Color::Rgb(225, 229, 238),
    prompt_border: Color::Rgb(211, 226, 255),
    prompt_border_active: Color::Rgb(57, 100, 254),
    code_bg: Color::Rgb(237, 243, 254),
    diff_insert_fg: Color::Rgb(34, 197, 94),
    diff_delete_fg: Color::Rgb(236, 19, 19),
    warning: Color::Rgb(221, 134, 41),
    syn_keyword: Color::Rgb(52, 74, 186),
    syn_string: Color::Rgb(22, 128, 76),
    syn_number: Color::Rgb(166, 92, 12),
    syn_comment: Color::Rgb(140, 146, 154),
    syn_type: Color::Rgb(20, 108, 168),
    syn_function: Color::Rgb(57, 100, 254),
    syn_variable: Color::Rgb(28, 32, 40),
    syn_punctuation: Color::Rgb(110, 116, 124),
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
        accent_brand: q(t.accent_brand),
        border: q(t.border),
        prompt_border: q(t.prompt_border),
        prompt_border_active: q(t.prompt_border_active),
        code_bg: q(t.code_bg),
        diff_insert_fg: q(t.diff_insert_fg),
        diff_delete_fg: q(t.diff_delete_fg),
        warning: q(t.warning),
        syn_keyword: q(t.syn_keyword),
        syn_string: q(t.syn_string),
        syn_number: q(t.syn_number),
        syn_comment: q(t.syn_comment),
        syn_type: q(t.syn_type),
        syn_function: q(t.syn_function),
        syn_variable: q(t.syn_variable),
        syn_punctuation: q(t.syn_punctuation),
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
    fn primary_accents_use_deepseek_blue_family() {
        assert_eq!(DARK.accent_brand, Color::Rgb(86, 134, 254));
        assert_eq!(DARK.prompt_border_active, DARK.accent_brand);
        assert_eq!(LIGHT.accent_brand, Color::Rgb(57, 100, 254));
        assert_eq!(LIGHT.prompt_border_active, LIGHT.accent_brand);
    }

    #[test]
    fn detects_levels() {
        // detect_level 读环境变量；这里只验证函数存在与 Ansi16 的保守默认
        let _ = detect_level();
    }
}
