use indextree::NodeId;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::Wrap;
use ratatui::widgets::canvas::{Canvas, Circle, Line as CanvasLine, Rectangle};
use ratatui::widgets::{
    Axis, BarChart, Block, Cell, Chart, Clear, Dataset, Fill, Gauge, GraphType, LineGauge, List,
    ListItem, Paragraph, Row, Scrollbar, ScrollbarOrientation, ScrollbarState, Sparkline, Table,
    Tabs,
};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::time::Instant;

use crate::gateway::{ANIM_MS, DragState, Gateway, InsertAt, lerp_rect};
use crate::tmss;
use crate::types::{Direction, Kind, Props, parse_width};
use terminism_kernel::host::Delivery;

pub(crate) fn draw(g: &mut Gateway, frame: &mut ratatui::Frame) {
    let area = frame.area();
    let main = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    if area.height > 0 {
        let status = Rect {
            x: area.x,
            y: area.y + area.height - 1,
            width: area.width,
            height: 1,
        };
        draw_status_bar(g, frame, status);
    }
    let page = g.current_page();
    // announce main-area size to the current page owner on first frame,
    // on size change, and whenever a different owner's page becomes current
    let owner = g.arena[page].get().owner.clone();
    let page_path = g.arena[page].get().path.clone();
    if g.last_size
        .as_ref()
        .is_none_or(|(s, o)| *s != (main.width, main.height) || *o != owner)
    {
        g.last_size = Some(((main.width, main.height), owner.clone()));
        g.pending.push(Delivery {
            target: owner,
            event: "ui.resize".to_string(),
            payload: serde_json::json!({
                "page": page_path,
                "width": main.width,
                "height": main.height,
            }),
        });
    }
    let mut rects = HashMap::new();
    collect_rects(g, page, main, &mut rects);
    // the dragged pane floats over the page at the cursor; its rect becomes
    // last_rects so the drop anim eases from the release position
    if let DragState::Drag {
        path,
        cursor,
        grab,
        size,
        ..
    } = &g.drag
        && g.paths.contains_key(path)
    {
        let w = size.0.min(area.width);
        let h = size.1.min(main.height);
        let x = cursor
            .0
            .saturating_sub(grab.0)
            .min(area.width.saturating_sub(w));
        let y = cursor
            .1
            .saturating_sub(grab.1)
            .min(main.height.saturating_sub(h));
        rects.insert(
            path.clone(),
            Rect {
                x,
                y,
                width: w,
                height: h,
            },
        );
    }
    g.update_anims(&rects);
    g.last_rects = rects;
    let mut finished = Vec::new();
    render_node(g, frame, page, main, Instant::now(), &mut finished);
    if let DragState::Drag { path, .. } = &g.drag
        && let Some(&id) = g.paths.get(path)
        && let Some(rect) = g.last_rects.get(path).copied()
    {
        frame.render_widget(Clear, rect);
        render_node(g, frame, id, rect, Instant::now(), &mut finished);
    }
    // the dashed slot ring paints on top of everything except the floating pane
    draw_drop_marker(g, frame);
    // an open select overlays its options below the control and records
    // one hit zone per option row
    if let Some(open) = g.select_open.clone()
        && let Some(&id) = g.paths.get(&open)
    {
        let titles = g.arena[id].get().props.titles.clone().unwrap_or_default();
        if let Some(rect) = g.last_rects.get(&open).copied() {
            g.select_hits.clear();
            let height = frame.area().height;
            // the menu hugs its content: longest option plus marker and padding,
            // clamped to the control and the screen, never a full-width band
            let content = titles
                .iter()
                .map(|t| t.chars().count() + 2)
                .max()
                .unwrap_or(0) as u16
                + 2;
            let menu_w = content
                .min(rect.width)
                .min(frame.area().width.saturating_sub(rect.x));
            let buf = frame.buffer_mut();
            for (idx, title) in titles.iter().enumerate() {
                let y = rect.y + 1 + idx as u16;
                if y >= rect.bottom() + titles.len().min(6) as u16 || y >= height {
                    break;
                }
                let mark = if idx == g.select_pending {
                    "● "
                } else {
                    "  "
                };
                let mut x = rect.x;
                let text = format!("{mark}{title}");
                for ch in text.chars().take(menu_w as usize) {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_symbol(ch.to_string().as_str());
                        if idx == g.select_pending {
                            cell.set_fg(Color::White);
                        } else {
                            cell.set_fg(Color::Gray);
                        }
                    }
                    x += 1;
                }
                while x < rect.x + menu_w {
                    if let Some(cell) = buf.cell_mut((x, y)) {
                        cell.set_symbol(" ");
                    }
                    x += 1;
                }
                g.select_hits
                    .push((open.clone(), idx, rect.x, rect.x + menu_w, y));
            }
        }
    } else {
        g.select_hits.clear();
    }
    g.anim.retain(|a| !finished.contains(&a.path));
}

// left: base keys plus the focused node's tmss hint; right: one glyph per
// Page (tmss icon), the current page un-reversed against the bar
fn draw_status_bar(g: &mut Gateway, frame: &mut ratatui::Frame, status: Rect) {
    let rev = Style::new().reversed();
    let mut left = tmss::i18n_text(&g.i18n, "status.keys");
    if let Some(focus) = g.focus.clone()
        && let Some(&id) = g.paths.get(&focus)
    {
        let node = g.arena[id].get();
        let ts = tmss::resolve(g.theme.borrow().rules(), &focus, node.kind, false, true);
        if let Some(hint) = ts.hint {
            left = format!("{left} · {hint}");
        }
    }
    let current = g.arena[g.current_page()].get().path.clone();
    let tabs: Vec<(String, String, bool)> = g
        .root
        .children(&g.arena)
        .filter(|&c| g.arena[c].get().kind == Kind::Page)
        .map(|c| g.arena[c].get().path.clone())
        .filter_map(|path| {
            let icon =
                tmss::resolve(g.theme.borrow().rules(), &path, Kind::Page, false, false).icon?;
            let is_current = path == current;
            Some((path, icon, is_current))
        })
        .collect();
    let strip_width = tabs.len() as u16 * 3;
    // display width, not char count: CJK labels are two columns per glyph and
    // a char-count pad would push the tab strip off its hit zones
    let left_w = Line::from(left.clone()).width() as u16;
    let pad = status.width.saturating_sub(left_w + strip_width);
    let mut spans = vec![
        Span::styled(left, rev),
        Span::styled(" ".repeat(pad as usize), rev),
    ];
    g.tab_hits.clear();
    let tab_x0 = status.x + status.width - strip_width;
    for (i, (path, icon, is_current)) in tabs.iter().enumerate() {
        let style = if *is_current { Style::new() } else { rev };
        spans.push(Span::styled(format!(" {icon} "), style));
        let x0 = tab_x0 + i as u16 * 3;
        g.tab_hits.push((path.clone(), x0, x0 + 3, status.y));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), status);
}

// dashed black border around the slot the drop will occupy; drawn over the
// floating pane so the target stays visible
fn draw_drop_marker(g: &Gateway, frame: &mut ratatui::Frame) {
    let DragState::Drag {
        parent,
        path,
        pending,
        ..
    } = &g.drag
    else {
        return;
    };
    let Some(&split) = g.paths.get(parent) else {
        return;
    };
    let (Some(&drag), Some(area)) = (g.paths.get(path), g.last_rects.get(parent).copied()) else {
        return;
    };
    // children in the order the drop produces, then plain flex math for the slot
    let mut kids: Vec<NodeId> = split
        .children(&g.arena)
        .filter(|&c| g.arena[c].get().path != *path)
        .collect();
    match pending {
        InsertAt::Before(sib) => {
            let pos = kids
                .iter()
                .position(|&c| g.arena[c].get().path == *sib)
                .unwrap_or(kids.len());
            kids.insert(pos, drag);
        }
        InsertAt::End => kids.push(drag),
    }
    let row = g.arena[split].get().direction != Some(Direction::Col);
    let flexes: Vec<u32> = kids
        .iter()
        .map(|&c| ((g.arena[c].get().props.flex.unwrap_or(1.0) * 1000.0) as u32).max(1))
        .collect();
    let total: u32 = flexes.iter().sum();
    let constraints: Vec<Constraint> = flexes
        .iter()
        .map(|f| Constraint::Ratio(*f, total))
        .collect();
    let areas = if row {
        Layout::horizontal(constraints).split(area)
    } else {
        Layout::vertical(constraints).split(area)
    };
    let Some(idx) = kids.iter().position(|&c| c == drag) else {
        return;
    };
    let slot = areas[idx];
    let buf = frame.buffer_mut();
    // a dashed gray ring on top of everything: solid glyphs outside the
    // floating pane, a gray tint over the pane's own glyphs inside it, so the
    // ring stays visible even when the pane covers its target slot
    let floating = g.last_rects.get(path).copied();
    let inside = |x: u16, y: u16| {
        floating.is_some_and(|f| x >= f.x && x < f.x + f.width && y >= f.y && y < f.y + f.height)
    };
    let bottom = slot.bottom().saturating_sub(1);
    let right = slot.right().saturating_sub(1);
    let mut edge = |x: u16, y: u16, glyph: &str| {
        let Some(cell) = buf.cell_mut((x, y)) else {
            return;
        };
        if inside(x, y) {
            cell.set_fg(Color::Rgb(0x9A, 0xA0, 0xA6));
        } else {
            cell.set_symbol(glyph).set_fg(Color::Rgb(0x9A, 0xA0, 0xA6));
        }
    };
    edge(slot.x, slot.y, "┌");
    edge(right, slot.y, "┐");
    edge(slot.x, bottom, "└");
    edge(right, bottom, "┘");
    for x in slot.x + 1..right {
        if x % 2 == 0 {
            edge(x, slot.y, "─");
            edge(x, bottom, "─");
        }
    }
    for y in slot.y + 1..bottom {
        if y % 2 == 0 {
            edge(slot.x, y, "│");
            edge(right, y, "│");
        }
    }
}

// hit rects must mirror what render_node paints, or hover and clicks land off target
fn collect_rects(g: &Gateway, id: NodeId, rect: Rect, out: &mut HashMap<String, Rect>) {
    let kind = g.arena[id].get().kind;
    out.insert(g.arena[id].get().path.clone(), rect);
    if kind == Kind::List {
        // items render one line each inside the border; overflow items are not hit-testable
        let inner = inner_rect(rect);
        let mut row = inner.y;
        for child in id.children(&g.arena) {
            if !matches!(g.arena[child].get().kind, Kind::Item | Kind::Slider) {
                continue;
            }
            if row >= inner.bottom() {
                break;
            }
            collect_rects(
                g,
                child,
                Rect {
                    x: inner.x,
                    y: row,
                    width: inner.width,
                    height: 1,
                },
                out,
            );
            row += 1;
        }
        return;
    }
    // bordered containers lay children out in their inner area, like render does
    let base = if matches!(kind, Kind::Section | Kind::Popup) {
        inner_rect(rect)
    } else {
        rect
    };
    for (child, crect) in children_rects(g, id, base) {
        collect_rects(g, child, crect, out);
    }
}

fn inner_rect(r: Rect) -> Rect {
    Rect {
        x: r.x.saturating_add(1),
        y: r.y.saturating_add(1),
        width: r.width.saturating_sub(2),
        height: r.height.saturating_sub(2),
    }
}

// non-Split parents divide their rect equally among children (Popup centers itself); Split by flex
// while dragging, the split's other children close the gap left by the floating pane
fn preview_children(g: &Gateway, split: NodeId) -> Vec<NodeId> {
    let kids: Vec<NodeId> = split.children(&g.arena).collect();
    let DragState::Drag { parent, path, .. } = &g.drag else {
        return kids;
    };
    if g.arena[split].get().path != *parent {
        return kids;
    }
    kids.into_iter()
        .filter(|&c| g.arena[c].get().path != *path)
        .collect()
}

fn children_rects(g: &Gateway, id: NodeId, rect: Rect) -> Vec<(NodeId, Rect)> {
    let kids: Vec<NodeId> = if g.arena[id].get().kind == Kind::Split {
        preview_children(g, id)
    } else {
        id.children(&g.arena).collect()
    };
    if kids.is_empty() {
        return Vec::new();
    }
    if g.arena[id].get().kind == Kind::Split {
        let row = g.arena[id].get().direction != Some(Direction::Col);
        let flexes: Vec<u32> = kids
            .iter()
            .map(|&c| ((g.arena[c].get().props.flex.unwrap_or(1.0) * 1000.0) as u32).max(1))
            .collect();
        let total: u32 = flexes.iter().sum();
        let constraints: Vec<Constraint> = flexes
            .iter()
            .map(|f| Constraint::Ratio(*f, total))
            .collect();
        let areas = if row {
            Layout::horizontal(constraints).split(rect)
        } else {
            Layout::vertical(constraints).split(rect)
        };
        return kids.into_iter().zip(areas.iter().copied()).collect();
    }
    // popups float over the parent instead of taking a share of its layout
    let (popups, normal): (Vec<_>, Vec<_>) = kids
        .into_iter()
        .partition(|&child| g.arena[child].get().kind == Kind::Popup);
    let mut out = Vec::with_capacity(normal.len() + popups.len());
    if !normal.is_empty() {
        let areas = Layout::vertical(vec![
            Constraint::Ratio(1, normal.len() as u32);
            normal.len()
        ])
        .split(rect);
        out.extend(normal.into_iter().zip(areas.iter().copied()));
    }
    out.extend(popups.into_iter().map(|child| (child, popup_center(rect))));
    out
}

fn popup_center(r: Rect) -> Rect {
    let width = r.width * 6 / 10;
    let height = r.height * 4 / 10;
    Rect {
        x: r.x + (r.width - width) / 2,
        y: r.y + (r.height - height) / 2,
        width,
        height,
    }
}

fn effective(
    g: &Gateway,
    path: &str,
    base: Rect,
    now: Instant,
    finished: &mut Vec<String>,
) -> Rect {
    match g.anim.iter().find(|a| a.path == path) {
        Some(a) => {
            if now.duration_since(a.start).as_millis() >= ANIM_MS {
                finished.push(path.to_string());
            }
            lerp_rect(a, now)
        }
        None => base,
    }
}

fn render_node(
    g: &Gateway,
    frame: &mut ratatui::Frame,
    id: NodeId,
    rect: Rect,
    now: Instant,
    finished: &mut Vec<String>,
) {
    let node = g.arena[id].get();
    let kind = node.kind;
    let path = &node.path;
    let props = &node.props;
    let eff = effective(g, path, rect, now, finished);
    let focused = g.focus.as_deref() == Some(path.as_str());
    let hovered = g.hover.as_deref() == Some(path.as_str());
    let dragged = matches!(&g.drag, DragState::Drag { path: p, .. } if p == path);
    // layering: inline props beat theme rules; drag feedback is a white tint only
    let ts = tmss::resolve(g.theme.borrow().rules(), path, kind, hovered, focused);
    let fg = props.color.as_deref().and_then(parse_color).or(ts.color);
    // drag feedback is a white tint only: bordered kinds keep their border, content stays normal
    let style = match (fg, ts.bg, dragged) {
        (_, _, true) => Some(Style::new().fg(Color::White)),
        (Some(f), Some(b), false) => Some(Style::new().fg(f).bg(b)),
        (Some(f), None, false) => Some(Style::new().fg(f)),
        (None, Some(b), false) => Some(Style::new().bg(b)),
        (None, None, false) => None,
    };
    // List and Table consume their children; Popup lays children out in its own inner area
    let consumes_children = matches!(kind, Kind::List | Kind::Table);
    match kind {
        Kind::Page | Kind::Split | Kind::Row => {}
        Kind::Fill => {
            let mut fill = Fill::new(" ");
            if let Some(s) = style {
                fill = fill.style(s);
            }
            frame.render_widget(fill, eff);
        }
        Kind::Section => {
            let mut block = Block::bordered();
            if let Some(title) = &props.title {
                block = block.title(title.clone());
            }
            if let Some(s) = style {
                block = block.style(s);
            }
            frame.render_widget(&block, eff);
            let inner = block.inner(eff);
            for (child, crect) in children_rects(g, id, inner) {
                render_node(g, frame, child, crect, now, finished);
            }
            return;
        }
        Kind::Label => {
            let mut text = props.text.clone().unwrap_or_default();
            if !text.is_empty()
                && let Some(glyph) = ts.fill
            {
                text = pad_center(&text, glyph, eff.width as usize);
            }
            let mut p = Paragraph::new(text).wrap(Wrap { trim: false });
            if let Some(a) = props
                .align
                .clone()
                .or_else(|| ts.align.map(str::to_string))
                .as_deref()
                .and_then(parse_align)
            {
                p = p.alignment(a);
            }
            if let Some(s) = style {
                p = p.style(s);
            }
            frame.render_widget(p, eff);
        }
        Kind::Item => {
            let text = format!("  {}", props.text.as_deref().unwrap_or_default());
            let mut p = Paragraph::new(text);
            if focused {
                p = p.style(Style::new().reversed());
            } else if hovered {
                p = p.style(Style::new().bg(Color::DarkGray));
            } else if let Some(s) = style {
                p = p.style(s);
            }
            frame.render_widget(p, eff);
        }
        Kind::Button => {
            let text = format!("[ {} ]", props.text.as_deref().unwrap_or_default());
            let mut p = Paragraph::new(text);
            if focused {
                p = p.style(Style::new().reversed());
            } else if hovered {
                p = p.style(Style::new().bg(Color::DarkGray));
            } else if let Some(s) = style {
                p = p.style(s);
            }
            frame.render_widget(p, eff);
        }
        Kind::TextInput => {
            render_text_input(g, frame, path, props, focused, style, eff);
        }
        Kind::Toggle => {
            let mark = if props.checked.unwrap_or(false) {
                "[✓] "
            } else {
                "[ ] "
            };
            let mut p = Paragraph::new(format!("{mark}{}", props.text.clone().unwrap_or_default()));
            if focused {
                p = p.style(Style::new().reversed());
            } else if hovered {
                p = p.style(Style::new().bg(Color::DarkGray));
            } else if let Some(s) = style {
                p = p.style(s);
            }
            frame.render_widget(p, eff);
        }
        Kind::Select => {
            let titles = props.titles.clone().unwrap_or_default();
            let len = titles.len();
            let sel = props
                .selected
                .map_or(0, |s| s.clamp(0, len as i64 - 1).max(0)) as usize;
            let current = titles
                .get(sel)
                .map_or_else(String::new, |t| format!("{t} ▾"));
            let mut p = Paragraph::new(current);
            if focused {
                p = p.style(Style::new().reversed());
            } else if hovered {
                p = p.style(Style::new().bg(Color::DarkGray));
            } else if let Some(s) = style {
                p = p.style(s);
            }
            frame.render_widget(p, eff);
        }
        Kind::List => render_list(g, frame, id, path, props, style, eff),
        Kind::Table => render_table(g, frame, id, props, style, eff),
        Kind::Tabs => {
            let titles = props.titles.clone().unwrap_or_default();
            let len = titles.len();
            let sel = props
                .selected
                .map_or(0, |s| s.clamp(0, len as i64 - 1).max(0)) as usize;
            let mut tabs = Tabs::new(titles)
                .select((len > 0).then_some(sel))
                .highlight_style(Style::new().reversed());
            if let Some(s) = style {
                tabs = tabs.style(s);
            }
            frame.render_widget(tabs, eff);
        }
        Kind::Slider => {
            let line_style = if focused {
                Style::new().fg(Color::Cyan)
            } else {
                style.unwrap_or_default()
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    slider_bar(props, eff.width),
                    line_style,
                ))),
                eff,
            );
        }
        Kind::Gauge => {
            let mut gauge = Gauge::default().ratio(f64::from(props.value.unwrap_or(0.0)));
            if let Some(text) = &props.text {
                gauge = gauge.label(text.clone());
            }
            if let Some(s) = style {
                gauge = gauge.gauge_style(s);
            }
            frame.render_widget(gauge, eff);
        }
        Kind::LineGauge => {
            let mut gauge = LineGauge::default().ratio(f64::from(props.value.unwrap_or(0.0)));
            if let Some(text) = &props.text {
                gauge = gauge.label(text.clone());
            }
            if let Some(s) = style {
                gauge = gauge.style(s);
            }
            frame.render_widget(gauge, eff);
        }
        Kind::BarChart => {
            let Some(bars) = bar_data(props.data.as_ref()) else {
                render_invalid(frame, eff);
                return;
            };
            let mut chart = BarChart::default().data(&bars);
            if let Some(v) = props.value {
                chart = chart.max(f64::from(v) as u64);
            }
            if let Some(s) = style {
                chart = chart.bar_style(s);
            }
            frame.render_widget(chart, eff);
        }
        Kind::Chart => {
            let Some(series) = chart_data(props.data.as_ref()) else {
                render_invalid(frame, eff);
                return;
            };
            let points: Vec<Vec<(f64, f64)>> = series.iter().map(|(_, p)| p.clone()).collect();
            let datasets: Vec<Dataset> = series
                .iter()
                .zip(&points)
                .map(|((name, _), pts)| {
                    let mut ds = Dataset::default()
                        .name(name.clone())
                        .marker(symbols::Marker::Braille)
                        .graph_type(GraphType::Line)
                        .data(pts);
                    if let Some(s) = style {
                        ds = ds.style(s);
                    }
                    ds
                })
                .collect();
            let (xb, yb) = chart_bounds(&points);
            let chart = Chart::new(datasets)
                .x_axis(Axis::default().bounds(xb))
                .y_axis(Axis::default().bounds(yb));
            frame.render_widget(chart, eff);
        }
        Kind::Sparkline => {
            let Some(values) = spark_data(props.data.as_ref()) else {
                render_invalid(frame, eff);
                return;
            };
            let mut spark = Sparkline::default().data(values);
            if let Some(s) = style {
                spark = spark.style(s);
            }
            frame.render_widget(spark, eff);
        }
        Kind::Calendar => {
            let (cur_year, cur_month) = today();
            let year = props.year.unwrap_or(cur_year);
            let month = props.month.unwrap_or(cur_month);
            frame.render_widget(calendar_paragraph(year, month, style), eff);
        }
        Kind::Clock => {
            let mut p = Paragraph::new(utc_clock_now());
            if let Some(a) = props.align.as_deref().and_then(parse_align) {
                p = p.alignment(a);
            }
            if let Some(s) = style {
                p = p.style(s);
            }
            frame.render_widget(p, eff);
        }
        Kind::Canvas => render_canvas(frame, props, style, eff),
        Kind::Popup => {
            frame.render_widget(Clear, eff);
            let mut block = Block::bordered();
            if let Some(title) = &props.title {
                block = block.title(title.clone());
            }
            if let Some(s) = style {
                block = block.style(s);
            }
            frame.render_widget(&block, eff);
            let inner = block.inner(eff);
            for (child, crect) in children_rects(g, id, inner) {
                render_node(g, frame, child, crect, now, finished);
            }
            return;
        }
    }
    if !consumes_children {
        for (child, crect) in children_rects(g, id, eff) {
            render_node(g, frame, child, crect, now, finished);
        }
    }
}

fn log_line_style(line: &str) -> Style {
    let upper = line.to_ascii_uppercase();
    if upper.contains("ERROR")
        || upper.contains("FATAL")
        || upper.contains("CRASH")
        || upper.contains("FAILED")
        || upper.contains("PANIC")
        || upper.contains("DENIED")
    {
        Style::new().fg(Color::Red)
    } else if upper.contains("WARN") || upper.contains("DROPPED") || upper.contains("FALLBACK") {
        Style::new().fg(Color::Yellow)
    } else if upper.contains("READY") || upper.contains("LOADED") || upper.contains("ACTIVE") {
        Style::new().fg(Color::Green)
    } else if upper.contains("DEBUG") || upper.contains("TRACE") {
        Style::new().fg(Color::DarkGray)
    } else {
        Style::new().fg(Color::Gray)
    }
}

fn slider_parts(props: &Props) -> (String, String) {
    let text = props.text.clone().unwrap_or_default();
    match text.split_once('|') {
        Some((label, value)) => (label.to_string(), value.to_string()),
        None => (text, String::new()),
    }
}

fn slider_bar(props: &Props, width: u16) -> String {
    let ratio = f64::from(props.value.unwrap_or(0.0).clamp(0.0, 1.0));
    let (label, value) = slider_parts(props);
    let label_width = label.chars().count();
    let value_width = value.chars().count();
    let label_space = usize::from(label_width > 0);
    let value_space = usize::from(value_width > 0);
    let bar_width = (width as usize)
        .saturating_sub(label_width + value_width + label_space + value_space)
        .max(3);
    let position = (ratio * (bar_width - 1) as f64).round() as usize;
    let mut bar = String::with_capacity(bar_width + label_width + value_width + 2);
    if label_width > 0 {
        bar.push_str(&label);
        bar.push(' ');
    }
    for index in 0..bar_width {
        if index == position {
            bar.push('●');
        } else if index < position {
            bar.push('━');
        } else {
            bar.push('─');
        }
    }
    if value_width > 0 {
        bar.push(' ');
        bar.push_str(&value);
    }
    bar
}

fn render_list(
    g: &Gateway,
    frame: &mut ratatui::Frame,
    id: NodeId,
    path: &str,
    props: &Props,
    style: Option<Style>,
    eff: Rect,
) {
    let visible = eff.height.saturating_sub(2) as usize;
    let count = if path == "/log/list" {
        g.log_store.borrow().len().min(100)
    } else {
        id.children(&g.arena)
            .filter(|&c| matches!(g.arena[c].get().kind, Kind::Item | Kind::Slider))
            .count()
    };
    let items: Vec<ListItem> = if path == "/log/list" {
        let sink = g.log_store.borrow();
        let start = sink.len().saturating_sub(100);
        sink.iter()
            .skip(start)
            .take(visible)
            .map(|l| ListItem::new(Line::from(Span::styled(l.clone(), log_line_style(l)))))
            .collect()
    } else {
        id.children(&g.arena)
            .filter(|&c| matches!(g.arena[c].get().kind, Kind::Item | Kind::Slider))
            .take(visible)
            .map(|c| {
                let node = g.arena[c].get();
                if node.kind == Kind::Slider {
                    let hovered = g.hover.as_deref() == Some(node.path.as_str());
                    let focused = g.focus.as_deref() == Some(node.path.as_str());
                    let ts = tmss::resolve(
                        g.theme.borrow().rules(),
                        &node.path,
                        Kind::Slider,
                        hovered,
                        focused,
                    );
                    let fg = node
                        .props
                        .color
                        .as_deref()
                        .and_then(parse_color)
                        .or(if focused { Some(Color::Cyan) } else { ts.color });
                    let mut s = fg.map_or_else(Style::new, |c| Style::new().fg(c));
                    if hovered {
                        s = s.bg(Color::DarkGray);
                    }
                    return ListItem::new(Line::from(Span::styled(
                        slider_bar(&node.props, eff.width.saturating_sub(2)),
                        s,
                    )));
                }
                let mut text = node.props.text.clone().unwrap_or_default();
                let item_ts = tmss::resolve(
                    g.theme.borrow().rules(),
                    &node.path,
                    Kind::Item,
                    false,
                    false,
                );
                if !text.is_empty()
                    && let Some(glyph) = item_ts.fill
                {
                    let inner = eff.width.saturating_sub(2) as usize;
                    text = format!("  {}", pad_center(text.trim(), glyph, inner));
                }
                let item = ListItem::new(text);
                let hovered = g.hover.as_deref() == Some(node.path.as_str());
                let focused = g.focus.as_deref() == Some(node.path.as_str());
                let ts = tmss::resolve(
                    g.theme.borrow().rules(),
                    &node.path,
                    Kind::Item,
                    hovered,
                    focused,
                );
                let fg = node
                    .props
                    .color
                    .as_deref()
                    .and_then(parse_color)
                    .or(ts.color);
                let mut s = fg.map_or_else(Style::new, |c| Style::new().fg(c));
                if let Some(b) = ts.bg {
                    s = s.bg(b);
                }
                if hovered {
                    s = s.bg(Color::DarkGray);
                }
                if focused {
                    s = s.reversed();
                }
                item.style(s)
            })
            .collect()
    };
    let mut block = Block::bordered();
    if let Some(title) = &props.title {
        block = block.title(title.clone());
    }
    if let Some(s) = style {
        block = block.style(s);
    }
    frame.render_widget(List::new(items).block(block), eff);
    overflow_scrollbar(frame, eff, count);
}

fn render_table(
    g: &Gateway,
    frame: &mut ratatui::Frame,
    id: NodeId,
    props: &Props,
    style: Option<Style>,
    eff: Rect,
) {
    let cols = props.columns.clone().unwrap_or_default();
    let widths: Vec<Constraint> = props.widths.as_ref().map_or_else(
        || vec![Constraint::Fill(1); cols.len()],
        |ws| ws.iter().filter_map(|w| parse_width(w)).collect(),
    );
    let rows: Vec<Row> = id
        .children(&g.arena)
        .filter(|&c| g.arena[c].get().kind == Kind::Row)
        .map(|c| {
            let node = g.arena[c].get();
            let cells = node.props.cells.clone().unwrap_or_default();
            let mut row = Row::new(cells.into_iter().map(Cell::from));
            if let Some(s) = node
                .props
                .color
                .as_deref()
                .and_then(parse_color)
                .map(|c| Style::new().fg(c))
            {
                row = row.style(s);
            }
            row
        })
        .collect();
    let count = rows.len() + 1;
    let mut block = Block::bordered();
    if let Some(title) = &props.title {
        block = block.title(title.clone());
    }
    let mut table = Table::new(rows, widths)
        .header(Row::new(cols.into_iter().map(Cell::from)))
        .block(block);
    if let Some(s) = style {
        table = table.style(s);
    }
    frame.render_widget(table, eff);
    overflow_scrollbar(frame, eff, count);
}

// visual only: List/Table scrolling is not interactive yet
fn overflow_scrollbar(frame: &mut ratatui::Frame, area: Rect, content_len: usize) {
    if content_len as u16 <= area.height.saturating_sub(2) {
        return;
    }
    frame.render_stateful_widget(
        Scrollbar::new(ScrollbarOrientation::VerticalRight),
        area,
        &mut ScrollbarState::new(content_len),
    );
}

fn render_text_input(
    g: &Gateway,
    frame: &mut ratatui::Frame,
    path: &str,
    props: &Props,
    focused: bool,
    style: Option<Style>,
    eff: Rect,
) {
    let input = &g.inputs[path];
    let value = input.value();
    let cursor = input.visual_cursor();
    if value.is_empty() && !focused {
        let placeholder = props.placeholder.clone().unwrap_or_default();
        frame.render_widget(
            Paragraph::new(placeholder).style(Style::new().fg(Color::DarkGray)),
            eff,
        );
        return;
    }
    let masked = props.mask.unwrap_or(false);
    let head: String = value
        .chars()
        .take(cursor)
        .map(|character| if masked { '*' } else { character })
        .collect();
    let tail: String = value
        .chars()
        .skip(cursor)
        .map(|character| if masked { '*' } else { character })
        .collect();
    let mut spans = vec![
        Span::raw(head),
        Span::styled("▏", Style::new().reversed()),
        Span::raw(tail),
    ];
    if focused {
        spans.insert(0, Span::raw("["));
        spans.push(Span::raw("]"));
    }
    let mut p = Paragraph::new(Line::from(spans));
    if let Some(s) = style {
        p = p.style(s);
    }
    frame.render_widget(p, eff);
}

fn render_canvas(frame: &mut ratatui::Frame, props: &Props, style: Option<Style>, eff: Rect) {
    let base = style.and_then(|s| s.fg);
    let canvas = Canvas::default()
        .marker(symbols::Marker::Braille)
        .x_bounds([0.0, 100.0])
        .y_bounds([0.0, 100.0])
        .paint(|ctx| {
            let Some(arr) = props.shapes.as_ref().and_then(JsonValue::as_array) else {
                return;
            };
            for entry in arr {
                let Some(obj) = entry.as_object() else {
                    continue;
                };
                let color = obj
                    .get("color")
                    .and_then(JsonValue::as_str)
                    .and_then(parse_color)
                    .or(base);
                let Some(color) = color else {
                    continue;
                };
                match obj.get("kind").and_then(JsonValue::as_str) {
                    Some("point") => {
                        if let (Some(x), Some(y)) = (num(obj, "x"), num(obj, "y")) {
                            ctx.print(x, y, Span::styled("●", Style::new().fg(color)));
                        }
                    }
                    Some("line") => {
                        if let (Some(x), Some(y), Some(x2), Some(y2)) =
                            (num(obj, "x"), num(obj, "y"), num(obj, "x2"), num(obj, "y2"))
                        {
                            ctx.draw(&CanvasLine {
                                x1: x,
                                y1: y,
                                x2,
                                y2,
                                color,
                            });
                        }
                    }
                    Some("rect") => {
                        if let (Some(x), Some(y), Some(x2), Some(y2)) =
                            (num(obj, "x"), num(obj, "y"), num(obj, "x2"), num(obj, "y2"))
                        {
                            ctx.draw(&Rectangle {
                                x: x.min(x2),
                                y: y.min(y2),
                                width: (x2 - x).abs(),
                                height: (y2 - y).abs(),
                                color,
                            });
                        }
                    }
                    Some("circle") => {
                        if let (Some(x), Some(y), Some(r)) =
                            (num(obj, "x"), num(obj, "y"), num(obj, "radius"))
                        {
                            ctx.draw(&Circle {
                                x,
                                y,
                                radius: r,
                                color,
                            });
                        }
                    }
                    _ => {}
                }
            }
        });
    frame.render_widget(canvas, eff);
}

fn render_invalid(frame: &mut ratatui::Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new("<invalid data>").style(Style::new().fg(Color::Red)),
        area,
    );
}

fn bar_data(data: Option<&JsonValue>) -> Option<Vec<(&str, u64)>> {
    let arr = data?.as_array()?;
    arr.iter()
        .map(|e| {
            let obj = e.as_object()?;
            Some((obj.get("label")?.as_str()?, obj.get("value")?.as_u64()?))
        })
        .collect()
}

type Series = Vec<(String, Vec<(f64, f64)>)>;

fn chart_data(data: Option<&JsonValue>) -> Option<Series> {
    let arr = data?.as_array()?;
    arr.iter()
        .map(|e| {
            let obj = e.as_object()?;
            let name = obj.get("name")?.as_str()?.to_string();
            let points = obj
                .get("points")?
                .as_array()?
                .iter()
                .map(|p| {
                    let pair = p.as_array()?;
                    Some((pair.first()?.as_f64()?, pair.get(1)?.as_f64()?))
                })
                .collect::<Option<Vec<_>>>()?;
            Some((name, points))
        })
        .collect()
}

fn spark_data(data: Option<&JsonValue>) -> Option<Vec<u64>> {
    let arr = data?.as_array()?;
    let values: Option<Vec<i64>> = arr.iter().map(|v| v.as_i64().map(|n| n.max(0))).collect();
    Some(values?.into_iter().map(|v| v as u64).collect())
}

fn chart_bounds(series: &[Vec<(f64, f64)>]) -> ([f64; 2], [f64; 2]) {
    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for points in series {
        for (x, y) in points {
            if *x < min_x {
                min_x = *x;
            }
            if *x > max_x {
                max_x = *x;
            }
            if *y < min_y {
                min_y = *y;
            }
            if *y > max_y {
                max_y = *y;
            }
        }
    }
    let fix = |min: f64, max: f64| {
        if min == f64::INFINITY || min == max {
            [0.0, 1.0]
        } else {
            [min, max]
        }
    };
    (fix(min_x, max_x), fix(min_y, max_y))
}

fn num(obj: &serde_json::Map<String, JsonValue>, key: &str) -> Option<f64> {
    obj.get(key).and_then(JsonValue::as_f64)
}

// month grid drawn by hand: ratatui's calendar widget needs time::Date and time
// is neither a dependency nor re-exported
fn calendar_paragraph(year: i64, month: i64, style: Option<Style>) -> Paragraph<'static> {
    const NAMES: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    const DAYS: &str = "Mo Tu We Th Fr Sa Su";
    let mut lines = vec![
        Line::from(format!("{} {}", NAMES[(month - 1) as usize], year)),
        Line::from(DAYS),
    ];
    let mut wd = weekday_mon0(days_from_civil(year, month, 1));
    let dim = days_in_month(year, month);
    let mut line = "   ".repeat(wd as usize);
    for day in 1..=dim {
        line.push_str(&format!("{day:>2} "));
        wd = (wd + 1) % 7;
        if wd == 0 {
            lines.push(Line::from(line.trim_end().to_string()));
            line.clear();
        }
    }
    if !line.trim_end().is_empty() {
        lines.push(Line::from(line.trim_end().to_string()));
    }
    let mut p = Paragraph::new(lines);
    if let Some(s) = style {
        p = p.style(s);
    }
    p
}

// days since 1970-01-01, proleptic Gregorian
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

// current UTC year and month, so Calendar can default to the present month
fn today() -> (i64, i64) {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64 / 86_400)
        .unwrap_or(0);
    civil_from_days(days)
}

// inverse of days_from_civil
fn civil_from_days(days: i64) -> (i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m)
}

fn weekday_mon0(days: i64) -> i64 {
    (days + 3).rem_euclid(7)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
            if leap { 29 } else { 28 }
        }
    }
}

// centers text and pads both sides with the fill glyph up to width
// HH:MM:SS UTC from the epoch clock; std has no local timezone
pub(crate) fn utc_clock_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day = secs % 86_400;
    format!(
        "{:02}:{:02}:{:02} UTC",
        day / 3600,
        day % 3600 / 60,
        day % 60
    )
}

fn pad_center(text: &str, glyph: char, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.to_string();
    }
    let fill = glyph.to_string();
    let total = width - len;
    let left = fill.repeat(total / 2);
    let right = fill.repeat(total - total / 2);
    format!("{left}{text}{right}")
}

fn parse_align(name: &str) -> Option<Alignment> {
    match name {
        "left" => Some(Alignment::Left),
        "center" => Some(Alignment::Center),
        _ => None,
    }
}

fn parse_color(name: &str) -> Option<Color> {
    // inline props accept the same palette as tmss, hex included
    crate::tmss::parse_color_public(name).ok()
}
