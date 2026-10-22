//! The link scope: the console's one memory point.
//!
//! A byte stream crossing a wire is what this product *is*, so the overview
//! page draws the link rather than describing it. Three elements, no chart
//! library:
//!
//! * a **timeline trace** of bytes per second in each direction over the last
//!   90 seconds, drawn as a filled envelope — the shape of traffic is what an
//!   operator reads at a glance (idle, burst, stall);
//! * a **savings ribbon** underneath: the fraction of bytes compression and
//!   diffing removed, drawn as a bar so a good link reads as a solid band;
//! * a **live readout** in the corner — current rate per direction — so the
//!   same information exists as a number for anyone who needs precision.
//!
//! The plot has a log-ish scale: desktop traffic spans three orders of
//! magnitude (a keystroke vs. a full-window redraw), and a linear axis would
//! make everything but the bursts invisible.

use std::cell::RefCell;
use std::rc::Rc;

use fltk::enums::{Align, Event};
use fltk::prelude::*;
use fltk::widget::Widget;

use crate::state::{fmt_rate, Sample, SCOPE_SAMPLES};
use crate::theme::{self as th, C, T};

/// What the scope is told to draw each frame.
#[derive(Clone, Default)]
pub struct ScopeData {
    pub samples: Vec<Sample>,
    /// Whether the link is currently live; drives the trace color.
    pub live: bool,
}

impl ScopeData {
    /// Peak rate in either direction, used to scale the axis. Rounded up to a
    /// "nice" value so the axis label is a round number.
    fn peak(&self) -> f64 {
        let max = self
            .samples
            .iter()
            .flat_map(|s| [s.sent_bps, s.recv_bps])
            .fold(0.0f64, f64::max);
        nice_ceiling(max)
    }
}

/// Round up to 1/2/5 × 10^n so the axis reads "500 KB/s" instead of
/// "473.2 KB/s".
fn nice_ceiling(v: f64) -> f64 {
    if v <= 0.0 {
        return 1024.0;
    }
    let exp = v.log10().floor();
    let base = 10f64.powf(exp);
    let m = v / base;
    let nice = if m <= 1.0 {
        1.0
    } else if m <= 2.0 {
        2.0
    } else if m <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * base
}

struct ScopeState {
    data: ScopeData,
    hover_x: Option<i32>,
}

/// The scope widget. Construct once per page; update with [`set_data`].
///
/// [`set_data`]: Scope::set_data
#[derive(Clone)]
pub struct Scope {
    w: Widget,
    st: Rc<RefCell<ScopeState>>,
}

impl Scope {
    pub fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        let st = Rc::new(RefCell::new(ScopeState {
            data: ScopeData::default(),
            hover_x: None,
        }));
        let mut wid = Widget::new(x, y, w, h, None);
        wid.set_frame(fltk::enums::FrameType::NoBox);

        let st_draw = st.clone();
        wid.draw(move |w| {
            let s = st_draw.borrow();
            paint(w.x(), w.y(), w.w(), w.h(), &s);
        });

        let st_ev = st.clone();
        wid.handle(move |w, ev| match ev {
            Event::Enter | Event::Move | Event::Drag => {
                let (mx, _) = fltk::app::event_coords();
                let mut s = st_ev.borrow_mut();
                s.hover_x = Some(mx - w.x());
                w.redraw();
                true
            }
            Event::Leave => {
                st_ev.borrow_mut().hover_x = None;
                w.redraw();
                true
            }
            _ => false,
        });

        Self { w: wid, st }
    }

    pub fn set_data(&self, data: ScopeData) {
        self.st.borrow_mut().data = data;
        self.w.clone().redraw();
    }

    pub fn resize(&self, x: i32, y: i32, w: i32, h: i32) {
        self.w.clone().resize(x, y, w, h);
    }
}

/// Plot geometry inside the widget box.
struct Plot {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// Height of the strip above the plot that carries the peak label and the
/// live readout. One text row plus a little air.
const TOP_GUTTER: i32 = 24;
/// Box height of that row's text: ink is centred in it.
const TOP_ROW_H: i32 = 16;

fn plot_rect(x: i32, y: i32, w: i32, h: i32) -> Plot {
    // Left gutter holds the axis labels, bottom gutter the time marks, and the
    // top gutter holds one row: the peak label and the live readout. It has to
    // be a full text row tall — at 14 the topmost axis label straddled the
    // plot's top edge and the readout sat 4 units off it.
    Plot {
        x: x + 58,
        y: y + TOP_GUTTER,
        w: (w - 58 - 14).max(10),
        h: (h - TOP_GUTTER - 22).max(10),
    }
}

fn paint(x: i32, y: i32, w: i32, h: i32, s: &ScopeState) {
    th::fill_round(x, y, w, h, crate::theme::L::RADIUS, C::SUNKEN);
    th::stroke_round(x, y, w, h, crate::theme::L::RADIUS, C::LINE);

    let p = plot_rect(x, y, w, h);
    let peak = s.data.peak();

    // Horizontal grid: three levels, the middle two labelled. A scope grid is
    // reference, not decoration — it exists so a height can be read.
    // The top row of the gutter: peak label on the left, live readout on the
    // right — both on ONE y, so the card's first line reads as a single row.
    let top_row_y = y + (TOP_GUTTER - TOP_ROW_H) / 2;
    for (i, frac) in [1.0f64, 0.5, 0.0].iter().enumerate() {
        let gy = p.y + p.h - (p.h as f64 * frac) as i32;
        th::hline(p.x, gy, p.w, if i == 2 { C::LINE_HI } else { C::LINE });
        let label = fmt_rate(peak * frac);
        // The peak line is the plot's top edge: its label belongs in the
        // gutter above, not straddling the edge. The other two sit on their
        // own gridline, which is the usual chart convention.
        let label_y = if i == 0 { top_row_y } else { gy - 8 };
        th::text(
            &label,
            x + 8,
            label_y,
            48,
            16,
            th::mono_font(),
            T::MICRO,
            C::TEXT_3,
            Align::Right,
        );
    }

    if s.data.samples.is_empty() {
        th::text(
            "等待链路数据",
            p.x,
            p.y,
            p.w,
            p.h,
            th::ui_font(),
            T::SMALL,
            C::TEXT_3,
            Align::Center,
        );
        paint_time_marks(x, y, w, h, &p);
        return;
    }

    let n = s.data.samples.len();
    let step = p.w as f64 / (SCOPE_SAMPLES.max(1) - 1) as f64;
    // Newest sample is right-aligned so the trace grows from the right edge.
    let x_of = |i: usize| -> i32 {
        let from_right = (n - 1 - i) as f64;
        (p.x as f64 + p.w as f64 - from_right * step).round() as i32
    };
    let y_of = |v: f64| -> i32 {
        let ratio = (v / peak).clamp(0.0, 1.0);
        p.y + p.h - (p.h as f64 * ratio).round() as i32
    };

    // Envelope fill: the receive trace is the filled body (it is the bulk of
    // a remote-desktop stream), the send trace is a brighter line on top.
    let trace = if s.data.live { C::AMBER } else { C::SAND };
    let recv_fill = th::mix(trace, C::SUNKEN, 0.24);
    let send_fill = th::mix(C::SAND, C::SUNKEN, 0.14);

    fill_envelope(
        &s.data.samples,
        &p,
        &x_of,
        &y_of,
        |sm| sm.recv_bps,
        recv_fill,
    );
    fill_envelope(
        &s.data.samples,
        &p,
        &x_of,
        &y_of,
        |sm| sm.sent_bps,
        send_fill,
    );

    th::push_clip(p.x, p.y, p.w, p.h);
    stroke_trace(
        &s.data.samples,
        &x_of,
        &y_of,
        |sm| sm.recv_bps,
        if s.data.live { C::AMBER } else { C::SAND },
        2,
    );
    stroke_trace(&s.data.samples, &x_of, &y_of, |sm| sm.sent_bps, C::SAND, 1);
    th::pop_clip();

    // Live readout, top-right of the plot: current values in figures.
    if let Some(last) = s.data.samples.last() {
        let r = format!("↓ {}", fmt_rate(last.recv_bps));
        let t = format!("↑ {}", fmt_rate(last.sent_bps));
        let rw = th::measure(&r, th::mono_font(), T::MICRO) + 10;
        th::text(
            &r,
            p.x + p.w - rw,
            top_row_y,
            rw,
            TOP_ROW_H,
            th::mono_font(),
            T::MICRO,
            C::TEXT_2,
            Align::Right,
        );
        let tw = th::measure(&t, th::mono_font(), T::MICRO) + 10;
        th::text(
            &t,
            p.x + p.w - rw - tw - 10,
            top_row_y,
            tw,
            TOP_ROW_H,
            th::mono_font(),
            T::MICRO,
            C::TEXT_3,
            Align::Right,
        );
    }

    paint_hover(&s.data, &p, s.hover_x, &x_of);
    paint_time_marks(x, y, w, h, &p);
}

/// Crosshair readout: hovering picks the sample under the pointer and shows
/// its exact rate, so the trace can be interrogated instead of estimated.
fn paint_hover<G>(data: &ScopeData, p: &Plot, hover_x: Option<i32>, x_of: &G)
where
    G: Fn(usize) -> i32,
{
    let Some(hx) = hover_x else { return };
    if hx < p.x || hx > p.x + p.w {
        return;
    }
    let n = data.samples.len();
    if n == 0 {
        return;
    }
    let step = p.w as f64 / (SCOPE_SAMPLES.max(1) - 1) as f64;
    let from_right = ((p.x + p.w - hx) as f64 / step).round() as usize;
    if from_right >= n {
        return;
    }
    let idx = n - 1 - from_right;
    let Some(sm) = data.samples.get(idx) else {
        return;
    };
    let px = x_of(idx);
    th::vline(px, p.y, p.h, C::TEXT_3);
    let label = format!(
        "{} · ↓ {} ↑ {}",
        idx_label(n, idx),
        fmt_rate(sm.recv_bps),
        fmt_rate(sm.sent_bps)
    );
    let lw = th::measure(&label, th::mono_font(), T::MICRO) + 14;
    let lx = (px + 8).min(p.x + p.w - lw);
    th::fill_round(lx, p.y + 20, lw, 20, 4, C::RAISED);
    th::mono(&label, lx + 6, p.y + 20, lw - 12, 20, T::MICRO, C::TEXT);
}

/// "-42s" style position label for the hovered sample.
fn idx_label(n: usize, idx: usize) -> String {
    let back = n - 1 - idx;
    if back == 0 {
        "现在".to_string()
    } else {
        // Samples are polled at 0.5 s.
        format!("-{:.1}s", back as f64 * 0.5)
    }
}

fn fill_envelope<F, G>(
    samples: &[Sample],
    p: &Plot,
    x_of: &G,
    y_of: &F,
    value: impl Fn(&Sample) -> f64,
    color: u32,
) where
    F: Fn(f64) -> i32,
    G: Fn(usize) -> i32,
{
    if samples.is_empty() {
        return;
    }
    th::push_clip(p.x, p.y, p.w, p.h);
    let mut prev: Option<(i32, i32)> = None;
    for (i, sm) in samples.iter().enumerate() {
        let x = x_of(i);
        let y = y_of(value(sm));
        if let Some((px, py)) = prev {
            // Fill the trapezoid under this segment down to the baseline.
            let base = p.y + p.h;
            th::fill(
                px.min(x),
                y.min(py),
                (x - px).abs().max(1),
                base - y.min(py),
                color,
            );
        }
        prev = Some((x, y));
    }
    th::pop_clip();
}

fn stroke_trace<F, G>(
    samples: &[Sample],
    x_of: &G,
    y_of: &F,
    value: impl Fn(&Sample) -> f64,
    color: u32,
    width: i32,
) where
    F: Fn(f64) -> i32,
    G: Fn(usize) -> i32,
{
    fltk::draw::set_draw_color(th::rgb(color));
    fltk::draw::set_line_style(fltk::draw::LineStyle::Solid, width);
    let mut prev: Option<(i32, i32)> = None;
    for (i, sm) in samples.iter().enumerate() {
        let x = x_of(i);
        let y = y_of(value(sm));
        if let Some((px, py)) = prev {
            if x != px {
                fltk::draw::draw_line(px, py, x, y);
            }
        }
        prev = Some((x, y));
    }
    fltk::draw::set_line_style(fltk::draw::LineStyle::Solid, 0);
}

fn paint_time_marks(x: i32, y: i32, w: i32, h: i32, p: &Plot) {
    let _ = (x, w);
    let marks = ["-90s", "-60s", "-30s", "现在"];
    let cw = p.w / 3;
    for (i, m) in marks.iter().enumerate() {
        let mx = p.x + cw * i as i32;
        th::hline(mx, p.y + p.h + 3, 1, C::LINE);
        th::text(
            m,
            mx - 12,
            y + h - 18,
            40,
            14,
            th::mono_font(),
            T::MICRO,
            C::TEXT_3,
            Align::Left,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_ceiling_snaps_to_round_numbers() {
        assert_eq!(nice_ceiling(473.2), 500.0);
        assert_eq!(nice_ceiling(1200.0), 2000.0);
        assert_eq!(nice_ceiling(0.0), 1024.0);
        assert_eq!(nice_ceiling(1.0), 1.0);
    }

    #[test]
    fn peak_uses_the_larger_direction() {
        let d = ScopeData {
            samples: vec![
                Sample {
                    sent_bps: 100.0,
                    recv_bps: 3000.0,
                },
                Sample {
                    sent_bps: 400.0,
                    recv_bps: 10.0,
                },
            ],
            ..ScopeData::default()
        };
        assert!(d.peak() >= 3000.0);
    }

    #[test]
    fn empty_scope_has_a_default_axis() {
        let d = ScopeData::default();
        assert_eq!(d.peak(), 1024.0);
    }
}
