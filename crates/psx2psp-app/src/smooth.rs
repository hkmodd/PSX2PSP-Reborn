//! Smooth wheel scrolling.
//!
//! iced's `scrollable` jumps 60 px per wheel notch in a single frame. [`Smooth`] wraps it, takes
//! the line-based wheel events (mouse wheels; touchpads already send pixel deltas and pass
//! through untouched) and replays them as small pixel steps on every frame, easing out
//! exponentially, so the panel glides instead of stepping.

use iced::advanced::layout::{self, Layout};
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget, mouse, overlay, renderer};
use iced::time::Instant;
use iced::{Element, Event, Length, Point, Rectangle, Size, Vector, window};

/// Distance of one wheel notch, in logical pixels (iced's own step is 60).
const NOTCH: f32 = 84.0;
/// Easing time constant: ~95 % of a notch is covered in 3 τ.
const TAU: f32 = 0.07;
/// Below this distance the remainder is applied at once and the animation stops.
const SETTLE: f32 = 0.5;

/// Wraps `content` (a scrollable) with eased wheel scrolling.
pub fn smooth<'a, Message: 'a, Theme: 'a, Renderer: renderer::Renderer + 'a>(
    content: impl Into<Element<'a, Message, Theme, Renderer>>,
) -> Smooth<'a, Message, Theme, Renderer> {
    Smooth {
        content: content.into(),
    }
}

pub struct Smooth<'a, Message, Theme, Renderer> {
    content: Element<'a, Message, Theme, Renderer>,
}

/// Wheel distance still to travel, where it was asked for and the last frame time.
#[derive(Debug, Default)]
struct State {
    pending: f32,
    at: Point,
    last: Option<Instant>,
}

impl State {
    /// Adds `notches` (positive = towards the top). A reversal drops what was left of the
    /// previous direction, so the panel answers at once.
    fn push(&mut self, notches: f32, at: Point) {
        let delta = notches * NOTCH;
        if delta * self.pending < 0.0 {
            self.pending = 0.0;
        }
        self.pending += delta;
        self.at = at;
    }

    /// The step for a frame `dt` seconds after the previous one.
    fn step(&mut self, dt: f32) -> f32 {
        let step = if self.pending.abs() <= SETTLE {
            self.pending
        } else {
            self.pending * (1.0 - (-dt / TAU).exp())
        };
        self.pending -= step;
        step
    }
}

impl<Message, Theme, Renderer: renderer::Renderer> Widget<Message, Theme, Renderer>
    for Smooth<'_, Message, Theme, Renderer>
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_ref(&self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Lines { x, y },
        }) = *event
            && x == 0.0
            && y != 0.0
            && let Some(at) = cursor.position_over(layout.bounds())
        {
            let state = tree.state.downcast_mut::<State>();
            if state.pending == 0.0 {
                state.last = None;
            }
            state.push(y, at);
            shell.capture_event();
            shell.request_redraw();
            return;
        }

        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        if let Event::Window(window::Event::RedrawRequested(now)) = *event {
            let state = tree.state.downcast_mut::<State>();
            if state.pending == 0.0 {
                return;
            }
            // The first frame after a quiet spell counts as one 60 Hz frame.
            let dt = state
                .last
                .map_or(1.0 / 60.0, |t| {
                    now.saturating_duration_since(t).as_secs_f32()
                })
                .min(0.05);
            state.last = Some(now);
            let step = state.step(dt);
            let wheel = Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: 0.0, y: step },
            });
            let at = mouse::Cursor::Available(state.at);
            self.content.as_widget_mut().update(
                &mut tree.children[0],
                &wheel,
                layout,
                at,
                renderer,
                clipboard,
                shell,
                viewport,
            );
            if tree.state.downcast_ref::<State>().pending != 0.0 {
                shell.request_redraw();
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

impl<'a, Message: 'a, Theme: 'a, Renderer: renderer::Renderer + 'a>
    From<Smooth<'a, Message, Theme, Renderer>> for Element<'a, Message, Theme, Renderer>
{
    fn from(s: Smooth<'a, Message, Theme, Renderer>) -> Self {
        Element::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::{NOTCH, SETTLE, State, TAU};
    use iced::Point;

    /// One notch at 60 fps travels the whole notch, front-loaded, in a few hundred ms.
    #[test]
    fn a_notch_eases_out_and_lands_exactly() {
        let mut s = State::default();
        s.push(-1.0, Point::ORIGIN);
        let steps: Vec<f32> = std::iter::from_fn(|| (s.pending != 0.0).then(|| s.step(1.0 / 60.0)))
            .take(1000)
            .collect();
        let total: f32 = steps.iter().sum();
        assert!((total + NOTCH).abs() < 1e-3, "{total}");
        assert!(
            steps.windows(2).all(|w| w[1].abs() <= w[0].abs() + SETTLE),
            "monotone ease-out"
        );
        let frames = steps.len() as f32 / 60.0;
        assert!(frames < TAU * 10.0, "settles in {frames} s");
    }

    #[test]
    fn notches_add_up_and_reversal_cancels() {
        let mut s = State::default();
        s.push(-1.0, Point::ORIGIN);
        s.push(-1.0, Point::ORIGIN);
        assert_eq!(s.pending, -2.0 * NOTCH);
        s.push(1.0, Point::ORIGIN);
        assert_eq!(s.pending, NOTCH, "reversal drops the old direction");
    }
}
