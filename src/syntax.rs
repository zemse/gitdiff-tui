use std::path::Path;
use std::sync::OnceLock;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SyntectStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

#[derive(Debug, Clone)]
pub struct Span {
    pub fg: (u8, u8, u8),
    pub bold: bool,
    pub italic: bool,
    pub text: String,
}

pub struct Highlighter {
    ps: SyntaxSet,
    theme: Theme,
}

static GLOBAL: OnceLock<Highlighter> = OnceLock::new();

impl Highlighter {
    pub fn global() -> &'static Highlighter {
        GLOBAL.get_or_init(|| {
            let ps = SyntaxSet::load_defaults_newlines();
            let ts = ThemeSet::load_defaults();
            let theme = ts
                .themes
                .get("base16-ocean.dark")
                .cloned()
                .unwrap_or_else(|| {
                    ts.themes
                        .values()
                        .next()
                        .cloned()
                        .expect("at least one theme")
                });
            Highlighter { ps, theme }
        })
    }

    pub fn syntax_for(&self, path: &str) -> &SyntaxReference {
        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        self.ps
            .find_syntax_by_extension(ext)
            .unwrap_or_else(|| self.ps.find_syntax_plain_text())
    }

    pub fn highlight(&self, path: &str, line: &str) -> Vec<Span> {
        let syntax = self.syntax_for(path);
        let mut h = HighlightLines::new(syntax, &self.theme);
        // syntect expects a trailing newline for stable parsing
        let line_with_nl = if line.ends_with('\n') {
            line.to_string()
        } else {
            format!("{line}\n")
        };
        match h.highlight_line(&line_with_nl, &self.ps) {
            Ok(ranges) => ranges
                .into_iter()
                .map(|(st, s)| convert_span(st, s))
                .collect(),
            Err(_) => vec![Span {
                fg: (200, 200, 200),
                bold: false,
                italic: false,
                text: line.to_string(),
            }],
        }
    }
}

fn convert_span(st: SyntectStyle, text: &str) -> Span {
    use syntect::highlighting::FontStyle;
    let trimmed = text.trim_end_matches('\n');
    Span {
        fg: (st.foreground.r, st.foreground.g, st.foreground.b),
        bold: st.font_style.contains(FontStyle::BOLD),
        italic: st.font_style.contains(FontStyle::ITALIC),
        text: trimmed.to_string(),
    }
}
