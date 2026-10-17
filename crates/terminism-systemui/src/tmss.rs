// tmss: a css-like stylesheet for node styling. Selectors combine a path and/or
// a kind per sequence, sequences compose by path-prefix descendant matching,
// and :hover/:focused gate a rule on interaction state. Inline props win over
// any rule; among rules, higher specificity wins (path segments, then kind
// count, then pseudo count), later files/rules beat earlier ones on ties.

use crate::types::Kind;
use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pseudo {
    Hover,
    Focused,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seq {
    pub path: Option<String>,
    pub kind: Option<Kind>,
    pub pseudo: Option<Pseudo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct TmssStyle {
    pub color: Option<Color>,
    pub bg: Option<Color>,
    pub align: Option<&'static str>,
    // yes pins the pane: it cannot be picked up by a drag (clicks still work)
    pub locked: Option<bool>,
    // appended to the status bar while the node is focused
    pub hint: Option<String>,
    // one glyph identifying a Page, shown in the status bar tab strip
    pub icon: Option<String>,
    // pads the text out to the node's rendered width with this glyph
    pub fill: Option<char>,
}

// opaque outside the crate: shells only parse and pass rules through
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub(crate) seqs: Vec<Seq>,
    pub(crate) style: TmssStyle,
    specificity: (usize, usize, usize),
}

// shared rule set: the styles-manager swaps the whole content at runtime
pub type Theme = std::rc::Rc<std::cell::RefCell<ThemeState>>;

#[derive(Default)]
pub struct ThemeState {
    rules: Vec<Rule>,
    revision: u64,
}

impl ThemeState {
    pub fn new(rules: Vec<Rule>) -> Self {
        Self { rules, revision: 0 }
    }
    pub fn rules(&self) -> &[Rule] {
        &self.rules
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn replace(&mut self, rules: Vec<Rule>) {
        if self.rules != rules {
            self.rules = rules;
            self.revision += 1;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TmssError {
    #[error("{0}")]
    Syntax(String),
}

impl TmssStyle {
    fn is_empty(&self) -> bool {
        self.color.is_none()
            && self.bg.is_none()
            && self.align.is_none()
            && self.locked.is_none()
            && self.hint.is_none()
            && self.icon.is_none()
            && self.fill.is_none()
    }
}

pub fn parse(text: &str) -> Result<Vec<Rule>, TmssError> {
    let mut rules = Vec::new();
    let stripped = strip_comments(text);
    let mut rest = skip_ws(&stripped);
    loop {
        if rest.is_empty() {
            return Ok(rules);
        }
        let Some(brace) = rest.find('{') else {
            return Err(TmssError::Syntax("missing {".to_string()));
        };
        let seqs = parse_selectors(&rest[..brace])?;
        let Some(close) = rest[brace..].find('}').map(|i| brace + i) else {
            return Err(TmssError::Syntax("missing }".to_string()));
        };
        let style = parse_decls(&rest[brace + 1..close])?;
        if !seqs.is_empty() && !style.is_empty() {
            let specificity = specificity(&seqs);
            rules.push(Rule {
                seqs,
                style,
                specificity,
            });
        }
        rest = skip_ws(&rest[close + 1..]);
    }
}

// '#' starts a comment only outside double quotes, so hex colors survive
fn strip_comments(text: &str) -> String {
    text.lines()
        .map(|l| {
            let mut in_quote = false;
            let mut cut = None;
            for (i, ch) in l.char_indices() {
                match ch {
                    '"' => in_quote = !in_quote,
                    '#' if !in_quote => {
                        cut = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
            cut.map_or(l, |i| &l[..i])
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn skip_ws(s: &str) -> &str {
    s.trim_start()
}

// sequences separated by whitespace; each is [path|kind] with an optional :pseudo
fn parse_selectors(text: &str) -> Result<Vec<Seq>, TmssError> {
    let mut seqs = Vec::new();
    for part in text.split_whitespace() {
        let (base, pseudo) = match part.split_once(':') {
            Some((b, p)) => (
                b,
                Some(match p {
                    "hover" => Pseudo::Hover,
                    "focused" => Pseudo::Focused,
                    _ => return Err(TmssError::Syntax(format!("unknown pseudo {p}"))),
                }),
            ),
            None => (part, None),
        };
        let (path, kind) = if base.is_empty() {
            return Err(TmssError::Syntax(format!("empty selector in {part}")));
        } else if base.starts_with('/') {
            (Some(base.to_string()), None)
        } else {
            match crate::types::parse_kind(base) {
                Some(k) => (None, Some(k)),
                None => return Err(TmssError::Syntax(format!("unknown kind {base}"))),
            }
        };
        seqs.push(Seq { path, kind, pseudo });
    }
    if seqs.is_empty() {
        return Err(TmssError::Syntax("empty selector".to_string()));
    }
    Ok(seqs)
}

// declarations separated by ';', each "key: value"
fn parse_decls(text: &str) -> Result<TmssStyle, TmssError> {
    let mut style = TmssStyle::default();
    for decl in text.split(';') {
        let decl = decl.trim();
        if decl.is_empty() {
            continue;
        }
        let Some((key, value)) = decl.split_once(':') else {
            return Err(TmssError::Syntax(format!("bad declaration {decl}")));
        };
        let (key, value) = (key.trim(), unquote(value.trim()));
        match key {
            "color" => style.color = Some(parse_color(value)?),
            "bg" => style.bg = Some(parse_color(value)?),
            "align" => {
                style.align = Some(match value {
                    "left" => "left",
                    "center" => "center",
                    _ => return Err(TmssError::Syntax(format!("bad align {value}"))),
                });
            }
            "locked" => {
                style.locked = Some(match value {
                    "yes" => true,
                    "no" => false,
                    _ => return Err(TmssError::Syntax(format!("bad locked {value}"))),
                });
            }
            "hint" => style.hint = Some(value.to_string()),
            "icon" => {
                if value.chars().count() != 1 {
                    return Err(TmssError::Syntax(format!(
                        "icon must be one glyph: {value}"
                    )));
                }
                style.icon = Some(value.to_string());
            }
            "fill" => {
                let Some(glyph) = value.chars().next() else {
                    return Err(TmssError::Syntax("empty fill".to_string()));
                };
                if value.chars().count() != 1 {
                    return Err(TmssError::Syntax(format!(
                        "fill must be one glyph: {value}"
                    )));
                }
                style.fill = Some(glyph);
            }
            _ => return Err(TmssError::Syntax(format!("unknown property {key}"))),
        }
    }
    Ok(style)
}

// strips one pair of surrounding double quotes so values may contain spaces
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
}

// catalog lookup with the key itself as the fallback
pub(crate) fn i18n_text(store: &terminism_kernel::host::I18nStore, key: &str) -> String {
    store
        .borrow()
        .get(key)
        .cloned()
        .unwrap_or_else(|| key.to_string())
}

// shared with render.rs so inline props and tmss rules agree on the palette
pub(crate) fn parse_color_public(name: &str) -> Result<Color, TmssError> {
    parse_color(name)
}

fn parse_color(name: &str) -> Result<Color, TmssError> {
    // exact RGB escapes terminal palette slots like Tango Dark's white-ish 7
    if let Some(hex) = name.strip_prefix('#') {
        let byte = |s: &str| u8::from_str_radix(s, 16);
        if hex.len() == 6
            && hex.chars().all(|c| c.is_ascii_hexdigit())
            && let (r, rest) = hex.split_at(2)
            && let (g, b) = rest.split_at(2)
        {
            return Ok(Color::Rgb(
                byte(r).unwrap_or(0),
                byte(g).unwrap_or(0),
                byte(b).unwrap_or(0),
            ));
        }
        return Err(TmssError::Syntax(format!("bad hex color {name}")));
    }
    Ok(match name {
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        "gray" => Color::Gray,
        "white" => Color::White,
        "black" => Color::Black,
        "darkgray" => Color::DarkGray,
        _ => return Err(TmssError::Syntax(format!("unknown color {name}"))),
    })
}

fn seq_matches(seq: &Seq, path: &str, kind: Kind, hovered: bool, focused: bool) -> bool {
    match seq.pseudo {
        Some(Pseudo::Hover) if !hovered => return false,
        Some(Pseudo::Focused) if !focused => return false,
        _ => {}
    }
    if let Some(p) = &seq.path
        && p != path
    {
        return false;
    }
    if let Some(k) = seq.kind
        && k != kind
    {
        return false;
    }
    true
}

fn path_segments(path: &str) -> usize {
    path.split('/').filter(|s| !s.is_empty()).count()
}

// the last sequence matches the node itself; each earlier one must match a
// strict path-prefix of the node (ancestor), in order. Path sequences match by
// prefix equality; kind-only ancestor sequences match at any depth.
fn rule_matches(rule: &Rule, path: &str, kind: Kind, hovered: bool, focused: bool) -> bool {
    let (last, ancestors) = rule.seqs.split_last().expect("selector non-empty");
    if !seq_matches(last, path, kind, hovered, focused) {
        return false;
    }
    let mut cursor = path;
    for seq in ancestors.iter().rev() {
        let mut matched = false;
        while let Some((parent, _seg)) = cursor.rsplit_once('/') {
            cursor = parent;
            if seq.path.as_deref() == Some(cursor) || seq.kind.is_some() {
                matched = true;
                break;
            }
        }
        if !matched {
            return false;
        }
    }
    true
}

fn specificity(seqs: &[Seq]) -> (usize, usize, usize) {
    seqs.iter().fold((0, 0, 0), |(p, k, s), seq| {
        (
            p + seq.path.as_ref().map_or(0, |x| path_segments(x)),
            k + usize::from(seq.kind.is_some()),
            s + usize::from(seq.pseudo.is_some()),
        )
    })
}

// each property resolves from the highest-specificity rule that sets it,
// later rules winning ties; rules that match but omit a property leave it alone
pub(crate) fn resolve(
    rules: &[Rule],
    path: &str,
    kind: Kind,
    hovered: bool,
    focused: bool,
) -> TmssStyle {
    let mut out = TmssStyle::default();
    let (mut color_best, mut bg_best, mut align_best, mut locked_best) = (None, None, None, None);
    let (mut hint_best, mut icon_best, mut fill_best) = (None, None, None);
    for rule in rules {
        if !rule_matches(rule, path, kind, hovered, focused) {
            continue;
        }
        let spec = rule.specificity;
        let s = &rule.style;
        if s.color.is_some() && color_best.as_ref().is_none_or(|k| spec >= *k) {
            color_best = Some(spec);
            out.color = s.color;
        }
        if s.bg.is_some() && bg_best.as_ref().is_none_or(|k| spec >= *k) {
            bg_best = Some(spec);
            out.bg = s.bg;
        }
        if s.align.is_some() && align_best.as_ref().is_none_or(|k| spec >= *k) {
            align_best = Some(spec);
            out.align = s.align;
        }
        if s.locked.is_some() && locked_best.as_ref().is_none_or(|k| spec >= *k) {
            locked_best = Some(spec);
            out.locked = s.locked;
        }
        if s.hint.is_some() && hint_best.as_ref().is_none_or(|k| spec >= *k) {
            hint_best = Some(spec);
            out.hint.clone_from(&s.hint);
        }
        if s.icon.is_some() && icon_best.as_ref().is_none_or(|k| spec >= *k) {
            icon_best = Some(spec);
            out.icon.clone_from(&s.icon);
        }
        if s.fill.is_some() && fill_best.as_ref().is_none_or(|k| spec >= *k) {
            fill_best = Some(spec);
            out.fill = s.fill;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(text: &str) -> Vec<Rule> {
        parse(text).unwrap()
    }

    #[test]
    fn parse_rules_kinds_paths_pseudos() {
        let rs = rules(
            "# c\nButton { color: white; }\nLabel:hover { bg: darkgray; }\n/home/hint { color: gray; }\n/plugins/split/list Item:focused { color: green; }\n",
        );
        assert_eq!(rs.len(), 4);
        assert_eq!(rs[0].seqs[0].kind, Some(Kind::Button));
        assert_eq!(rs[2].seqs[0].path.as_deref(), Some("/home/hint"));
        assert_eq!(rs[3].seqs.len(), 2);
        assert_eq!(rs[3].seqs[0].path.as_deref(), Some("/plugins/split/list"));
        assert_eq!(rs[3].seqs[1].pseudo, Some(Pseudo::Focused));
        assert_eq!(rs[1].style.bg, Some(Color::DarkGray));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse("Button { color: chartreuse; }").is_err());
        assert!(parse("NotAKind { color: white; }").is_err());
        assert!(parse("Button { color white; }").is_err());
        assert!(parse("Button:hover:dragged { color: white; }").is_err());
        assert!(parse("Button").is_err());
    }

    #[test]
    fn specificity_orders_rules() {
        let rs = rules(
            "Button { color: red; }\nButton { color: blue; }\n/hello/btn { color: green; }\n",
        );
        // kind loses to the exact path regardless of order
        let s = resolve(&rs, "/hello/btn", Kind::Button, false, false);
        assert_eq!(s.color, Some(Color::Green));
        // two kind rules: the later one wins
        let s = resolve(&rs, "/x/y", Kind::Button, false, false);
        assert_eq!(s.color, Some(Color::Blue));
    }

    #[test]
    fn pseudo_gates_matching() {
        let rs = rules("Button:hover { color: green; }\nButton { color: red; }");
        assert_eq!(
            resolve(&rs, "/b", Kind::Button, false, false).color,
            Some(Color::Red)
        );
        assert_eq!(
            resolve(&rs, "/b", Kind::Button, true, false).color,
            Some(Color::Green)
        );
    }

    #[test]
    fn descendant_requires_ancestor_prefix() {
        let rs = rules("/a/b Item { color: green; }");
        assert_eq!(
            resolve(&rs, "/a/b/c", Kind::Item, false, false).color,
            Some(Color::Green)
        );
        assert_eq!(resolve(&rs, "/x/b/c", Kind::Item, false, false).color, None);
    }

    #[test]
    fn descendant_kind_matches_any_depth() {
        let rs = rules("Split Item { color: green; }");
        assert_eq!(
            resolve(&rs, "/any/where/it", Kind::Item, false, false).color,
            Some(Color::Green)
        );
    }

    #[test]
    fn locked_parses_yes_no_only() {
        let rs = rules("/a/b { locked: yes; }");
        assert_eq!(
            resolve(&rs, "/a/b", Kind::Section, false, false).locked,
            Some(true)
        );
        assert!(parse("/a { locked: maybe; }").is_err());
    }

    #[test]
    fn properties_merge_across_rules() {
        // two same-specificity rules: each property survives from the rule that sets it
        let rs = rules("/a/b { color: gray; }\n/a/b { locked: yes; }\n/a/b { color: cyan; }\n");
        let s = resolve(&rs, "/a/b", Kind::Section, false, false);
        assert_eq!(s.color, Some(Color::Cyan));
        assert_eq!(s.locked, Some(true));
    }

    #[test]
    fn hex_colors_parse() {
        let rs = rules(
            "/a { color: \"#9aa0a6\"; bg: \"#FF00CC\"; }
",
        );
        let s = resolve(&rs, "/a", Kind::Label, false, false);
        assert_eq!(s.color, Some(Color::Rgb(0x9A, 0xA0, 0xA6)));
        assert_eq!(s.bg, Some(Color::Rgb(0xFF, 0x00, 0xCC)));
        assert!(parse("/a { color: \"#9AA0\"; }").is_err());
        assert!(parse("/a { color: \"#ZZZZZZ\"; }").is_err());
    }

    #[test]
    fn fill_parses_single_glyph() {
        let rs = rules(
            "/a { fill: \"=\"; }
",
        );
        assert_eq!(
            resolve(&rs, "/a", Kind::Label, false, false).fill,
            Some('=')
        );
        assert!(parse("/a { fill: \"==\"; }").is_err());
        assert!(parse("/a { fill: \"\"; }").is_err());
    }

    #[test]
    fn hint_and_icon_parse_quoted() {
        let rs = rules("/home { icon: \"⌂\"; }\nButton { hint: \"Enter activate\"; }\n");
        assert_eq!(
            resolve(&rs, "/home", Kind::Page, false, false)
                .icon
                .as_deref(),
            Some("⌂")
        );
        assert_eq!(
            resolve(&rs, "/b", Kind::Button, false, false)
                .hint
                .as_deref(),
            Some("Enter activate")
        );
        assert!(parse("/p { icon: \"ab\"; }").is_err());
    }
}
