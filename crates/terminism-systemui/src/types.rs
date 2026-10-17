use ratatui::layout::Constraint;

use terminism_kernel::host::Json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Page,
    Section,
    Split,
    Fill,
    Label,
    List,
    Item,
    Table,
    Row,
    Tabs,
    Gauge,
    LineGauge,
    BarChart,
    Chart,
    Sparkline,
    Calendar,
    Clock,
    Canvas,
    Button,
    TextInput,
    Popup,
    Toggle,
    Select,
    Slider,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Props {
    pub text: Option<String>,
    pub title: Option<String>,
    pub color: Option<String>,
    pub value: Option<f32>,
    pub data: Option<Json>,
    pub columns: Option<Vec<String>>,
    pub widths: Option<Vec<String>>,
    pub cells: Option<Vec<String>>,
    pub titles: Option<Vec<String>>,
    pub selected: Option<i64>,
    pub year: Option<i64>,
    pub month: Option<i64>,
    pub shapes: Option<Json>,
    pub flex: Option<f32>,
    pub placeholder: Option<String>,
    pub align: Option<String>,
    pub checked: Option<bool>,
    pub mask: Option<bool>,
}

impl Props {
    pub(crate) fn apply(&mut self, update: Self) -> bool {
        let mut changed = false;
        macro_rules! fields {
            ($($field:ident),*) => {$(
                if update.$field.is_some() && self.$field != update.$field {
                    self.$field = update.$field;
                    changed = true;
                }
            )*};
        }
        fields!(
            text,
            title,
            color,
            value,
            data,
            columns,
            widths,
            cells,
            titles,
            selected,
            year,
            month,
            shapes,
            flex,
            placeholder,
            align,
            checked,
            mask
        );
        changed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Row,
    Col,
}

pub struct NodeData {
    pub path: String,
    pub owner: String,
    pub kind: Kind,
    pub props: Props,
    pub direction: Option<Direction>,
}

pub fn parse_kind(name: &str) -> Option<Kind> {
    Some(match name {
        "Page" => Kind::Page,
        "Section" => Kind::Section,
        "Split" => Kind::Split,
        "Fill" => Kind::Fill,
        "Label" => Kind::Label,
        "List" => Kind::List,
        "Item" => Kind::Item,
        "Table" => Kind::Table,
        "Row" => Kind::Row,
        "Tabs" => Kind::Tabs,
        "Gauge" => Kind::Gauge,
        "LineGauge" => Kind::LineGauge,
        "BarChart" => Kind::BarChart,
        "Chart" => Kind::Chart,
        "Sparkline" => Kind::Sparkline,
        "Calendar" => Kind::Calendar,
        "Clock" => Kind::Clock,
        "Canvas" => Kind::Canvas,
        "Button" => Kind::Button,
        "TextInput" => Kind::TextInput,
        "Popup" => Kind::Popup,
        "Toggle" => Kind::Toggle,
        "Select" => Kind::Select,
        "Slider" => Kind::Slider,
        _ => return None,
    })
}

// create=true accepts the Split-only `direction` key; None rejects the whole op
pub fn parse_props(kind: Kind, json: &Json, create: bool) -> Option<(Props, Option<Direction>)> {
    let map = json.as_object()?;
    let mut props = Props::default();
    let mut direction = None;
    for (key, value) in map {
        match key.as_str() {
            "text"
                if matches!(
                    kind,
                    Kind::Label
                        | Kind::Item
                        | Kind::Button
                        | Kind::TextInput
                        | Kind::Gauge
                        | Kind::LineGauge
                        | Kind::Toggle
                        | Kind::Select
                        | Kind::Slider
                ) =>
            {
                props.text = Some(value.as_str()?.to_string());
            }
            "title"
                if matches!(
                    kind,
                    Kind::Page | Kind::Section | Kind::List | Kind::Table | Kind::Popup
                ) =>
            {
                props.title = Some(value.as_str()?.to_string());
            }
            "color" if !matches!(kind, Kind::Page | Kind::Split) => {
                props.color = Some(value.as_str()?.to_string());
            }
            "value" if matches!(kind, Kind::Gauge | Kind::LineGauge | Kind::Slider) => {
                let v = value.as_f64()? as f32;
                if !(0.0..=1.0).contains(&v) {
                    return None;
                }
                props.value = Some(v);
            }
            "value" if kind == Kind::BarChart => {
                let v = value.as_f64()?;
                if v.is_nan() || v < 0.0 {
                    return None;
                }
                props.value = Some(v as f32);
            }
            "data" if matches!(kind, Kind::BarChart | Kind::Chart | Kind::Sparkline) => {
                props.data = Some(value.clone());
            }
            "columns" if kind == Kind::Table => {
                props.columns = Some(str_vec(value)?);
            }
            "widths" if kind == Kind::Table => {
                let widths = str_vec(value)?;
                for width in &widths {
                    parse_width(width)?;
                }
                props.widths = Some(widths);
            }
            "cells" if kind == Kind::Row => {
                props.cells = Some(str_vec(value)?);
            }
            "titles" if matches!(kind, Kind::Tabs | Kind::Select) => {
                props.titles = Some(str_vec(value)?);
            }
            "selected" if matches!(kind, Kind::Tabs | Kind::Select) => {
                props.selected = Some(value.as_i64()?);
            }
            "year" if kind == Kind::Calendar => {
                props.year = Some(value.as_i64()?);
            }
            "month" if kind == Kind::Calendar => {
                let m = value.as_i64()?;
                if !(1..=12).contains(&m) {
                    return None;
                }
                props.month = Some(m);
            }
            "shapes" if kind == Kind::Canvas => {
                props.shapes = Some(value.clone());
            }
            "flex" => {
                let f = value.as_f64()? as f32;
                if f.is_nan() || f <= 0.0 {
                    return None;
                }
                props.flex = Some(f);
            }
            "placeholder" if kind == Kind::TextInput => {
                props.placeholder = Some(value.as_str()?.to_string());
            }
            "mask" if kind == Kind::TextInput => {
                props.mask = Some(value.as_bool()?);
            }
            "checked" if kind == Kind::Toggle => {
                props.checked = Some(value.as_bool()?);
            }
            "align" if matches!(kind, Kind::Label | Kind::Clock) => {
                let a = value.as_str()?;
                if a != "left" && a != "center" {
                    return None;
                }
                props.align = Some(a.to_string());
            }
            "direction" if kind == Kind::Split && create => {
                direction = Some(match value.as_str()? {
                    "row" => Direction::Row,
                    "col" => Direction::Col,
                    _ => return None,
                });
            }
            _ => return None,
        }
    }
    Some((props, direction))
}

// "10" fixed / "30%" percentage / "*" equal share
pub fn parse_width(text: &str) -> Option<Constraint> {
    if text == "*" {
        return Some(Constraint::Fill(1));
    }
    let digits = text.strip_suffix('%').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let n: u16 = digits.parse().ok()?;
    Some(if text.ends_with('%') {
        Constraint::Percentage(n)
    } else {
        Constraint::Length(n)
    })
}

fn str_vec(value: &Json) -> Option<Vec<String>> {
    let arr = value.as_array()?;
    arr.iter().map(|v| v.as_str().map(str::to_string)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn align_gate() {
        let (props, _) =
            parse_props(Kind::Label, &json!({"text": "t", "align": "center"}), true).unwrap();
        assert_eq!(props.align.as_deref(), Some("center"));
        assert!(parse_props(Kind::Item, &json!({"text": "t", "align": "center"}), true).is_none());
        assert!(parse_props(Kind::Label, &json!({"text": "t", "align": "right"}), true).is_none());
    }

    #[test]
    fn text_input_mask_parses() {
        let (props, _) = parse_props(
            Kind::TextInput,
            &json!({"text": "secret", "mask": true}),
            true,
        )
        .unwrap();
        assert_eq!(props.mask, Some(true));
        assert!(parse_props(Kind::Label, &json!({"text": "t", "mask": true}), true).is_none());
    }
}
