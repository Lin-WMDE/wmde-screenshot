// SPDX-License-Identifier: GPL-3.0-only

//! WMDE: the layer the person draws on, over the frame and under the toolbar.

use crate::editor::{Editor, Object, arrow_head};
use cosmic::iced::widget::canvas::{self, Geometry, LineCap, LineJoin, Path, Stroke};
use cosmic::iced::{Point, Rectangle, Size, mouse};

/// What the pointer did on the drawing layer. Turned into editor calls by the application
/// half, so that the drawing state lives in one place rather than inside a widget.
#[derive(Clone, Copy, Debug)]
pub enum Draw {
    Press(Point),
    Drag(Point),
    Release,
}

pub struct Annotations<'a> {
    pub editor: &'a Editor,
    /// Where this output sits in the desktop. Objects are kept in desktop coordinates, so
    /// a mark drawn on the second monitor stays on the second monitor - each output has
    /// its own surface and its own canvas, and they all share one editor.
    pub origin: cosmic::iced::Vector,
}

impl canvas::Program<Draw, cosmic::Theme, cosmic::Renderer> for Annotations<'_> {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        event: &cosmic::iced::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Draw>> {
        // With no tool in hand the layer is transparent to the pointer: the press belongs
        // to the selection underneath, which is what moves and resizes it.
        self.editor.tool?;

        let at = cursor.position_in(bounds).map(|at| at + self.origin);

        match event {
            cosmic::iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                at.map(|at| canvas::Action::publish(Draw::Press(at)).and_capture())
            }
            cosmic::iced::Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                at.map(|at| canvas::Action::publish(Draw::Drag(at)))
            }
            cosmic::iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                Some(canvas::Action::publish(Draw::Release).and_capture())
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &cosmic::Renderer,
        _theme: &cosmic::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry<cosmic::Renderer>> {
        if self.editor.is_empty() {
            return Vec::new();
        }

        let mut frame = canvas::Frame::new(renderer, bounds.size());
        // Desktop coordinates in, surface coordinates out.
        frame.translate(-self.origin);

        for object in self.editor.objects() {
            match object {
                Object::Path { points, stroke } => {
                    let Some((first, rest)) = points.split_first() else {
                        continue;
                    };
                    let path = Path::new(|builder| {
                        builder.move_to(*first);
                        for point in rest {
                            builder.line_to(*point);
                        }
                    });
                    frame.stroke(&path, pen(stroke.width, stroke.color));
                }
                Object::Line { from, to, stroke } => {
                    let path = Path::new(|builder| {
                        builder.move_to(*from);
                        builder.line_to(*to);
                    });
                    frame.stroke(&path, pen(stroke.width, stroke.color));
                }
                Object::Arrow { from, to, stroke } => {
                    let (left, right) = arrow_head(*from, *to, stroke.width);
                    let path = Path::new(|builder| {
                        builder.move_to(*from);
                        builder.line_to(*to);
                        builder.move_to(left);
                        builder.line_to(*to);
                        builder.line_to(right);
                    });
                    frame.stroke(&path, pen(stroke.width, stroke.color));
                }
                Object::Rectangle { from, to, stroke } => {
                    let top_left = Point::new(from.x.min(to.x), from.y.min(to.y));
                    let size = Size::new((to.x - from.x).abs(), (to.y - from.y).abs());
                    let path = Path::new(|builder| builder.rectangle(top_left, size));
                    frame.stroke(&path, pen(stroke.width, stroke.color));
                }
            }
        }

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        _state: &Self::State,
        _bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        match self.editor.tool {
            Some(_) => mouse::Interaction::Crosshair,
            None => mouse::Interaction::default(),
        }
    }
}

/// Round ends and joins on everything: a freehand stroke drawn with square caps shows
/// every point it was built from.
fn pen(width: f32, color: cosmic::iced::Color) -> Stroke<'static> {
    Stroke::default()
        .with_color(color)
        .with_width(width)
        .with_line_cap(LineCap::Round)
        .with_line_join(LineJoin::Round)
}
