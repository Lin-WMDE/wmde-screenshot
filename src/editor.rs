// SPDX-License-Identifier: GPL-3.0-only

//! WMDE: what gets drawn on top of a screenshot before it is saved.
//!
//! Two renderers draw the same objects. `canvas` draws them on screen while the person
//! works, and tiny-skia draws them into the image at save time. Neither is optional: a
//! preview that does not match the file is worse than no preview, so the shapes are
//! defined once here and emitted twice.

use cosmic::iced::{Color, Point};

/// What the pointer does inside the selection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tool {
    Pencil,
    Line,
    Arrow,
    Rectangle,
    Marker,
}

impl Tool {
    /// The key that picks this tool. Single letters, no modifiers: the overlay has the
    /// keyboard to itself, and reaching for a modifier to change pencil is a tax on every
    /// stroke.
    pub const fn key(self) -> char {
        match self {
            Self::Pencil => 'p',
            Self::Line => 'l',
            Self::Arrow => 'a',
            Self::Rectangle => 'r',
            Self::Marker => 'm',
        }
    }

    pub fn from_key(key: char) -> Option<Self> {
        [
            Self::Pencil,
            Self::Line,
            Self::Arrow,
            Self::Rectangle,
            Self::Marker,
        ]
        .into_iter()
        .find(|tool| tool.key() == key)
    }

    /// A marker is a wide translucent stroke; everything else draws at the chosen width.
    fn stroke(self, width: f32, color: Color) -> Stroke {
        match self {
            Self::Marker => Stroke {
                width: width * 4.0,
                color: Color { a: 0.35, ..color },
            },
            _ => Stroke { width, color },
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Stroke {
    pub width: f32,
    pub color: Color,
}

/// One drawn thing, in the coordinates of the surface it was drawn on.
#[derive(Clone, Debug)]
pub enum Object {
    /// Pencil and marker: the pointer's path, thinned as it is collected.
    Path { points: Vec<Point>, stroke: Stroke },
    Line {
        from: Point,
        to: Point,
        stroke: Stroke,
    },
    Arrow {
        from: Point,
        to: Point,
        stroke: Stroke,
    },
    /// Outline only. Filling a rectangle is what the blur is for.
    Rectangle {
        from: Point,
        to: Point,
        stroke: Stroke,
    },
}

/// The drawing half of the overlay.
pub struct Editor {
    /// Everything placed, oldest first. The order is the drawing order.
    objects: Vec<Object>,
    /// Taken off by undo, in the order they can be put back.
    undone: Vec<Object>,
    /// `None` means the pointer resizes and moves the selection instead of drawing.
    pub tool: Option<Tool>,
    pub color: Color,
    pub width: f32,
    /// The object under the pointer right now, not yet placed.
    drawing: Option<Object>,
}

impl Default for Editor {
    fn default() -> Self {
        Self {
            objects: Vec::new(),
            undone: Vec::new(),
            tool: None,
            // Red on a screenshot is what an arrow is for. The palette can move it.
            color: Color::from_rgb(0.90, 0.16, 0.16),
            width: 3.0,
            drawing: None,
        }
    }
}

impl Editor {
    /// Everything to draw, placed objects first and the one in hand last.
    pub fn objects(&self) -> impl Iterator<Item = &Object> {
        self.objects.iter().chain(self.drawing.iter())
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty() && self.drawing.is_none()
    }

    pub fn press(&mut self, at: Point) {
        let Some(tool) = self.tool else {
            return;
        };
        let stroke = tool.stroke(self.width, self.color);

        self.drawing = Some(match tool {
            Tool::Pencil | Tool::Marker => Object::Path {
                points: vec![at],
                stroke,
            },
            Tool::Line => Object::Line {
                from: at,
                to: at,
                stroke,
            },
            Tool::Arrow => Object::Arrow {
                from: at,
                to: at,
                stroke,
            },
            Tool::Rectangle => Object::Rectangle {
                from: at,
                to: at,
                stroke,
            },
        });
    }

    pub fn drag(&mut self, at: Point) {
        match &mut self.drawing {
            // Points closer together than this add nothing but memory: the stroke is
            // several pixels wide and a freehand line collects hundreds of events.
            Some(Object::Path { points, .. }) => {
                if points.last().is_none_or(|last| last.distance(at) > 2.0) {
                    points.push(at);
                }
            }
            Some(
                Object::Line { to, .. } | Object::Arrow { to, .. } | Object::Rectangle { to, .. },
            ) => *to = at,
            None => {}
        }
    }

    /// Finish the object in hand. A click that never moved leaves nothing behind - an
    /// empty line is not a mark, it is a missed press.
    pub fn release(&mut self) {
        let Some(object) = self.drawing.take() else {
            return;
        };

        let worth_keeping = match &object {
            Object::Path { points, .. } => points.len() > 1,
            Object::Line { from, to, .. }
            | Object::Arrow { from, to, .. }
            | Object::Rectangle { from, to, .. } => from.distance(*to) > 2.0,
        };

        if worth_keeping {
            self.objects.push(object);
            // A new mark ends the redo trail: what was undone belonged to a history that
            // no longer happened.
            self.undone.clear();
        }
    }

    pub fn undo(&mut self) {
        if let Some(object) = self.objects.pop() {
            self.undone.push(object);
        }
    }

    pub fn redo(&mut self) {
        if let Some(object) = self.undone.pop() {
            self.objects.push(object);
        }
    }
}

/// Where an arrow's head sits, given its line. Returned as the two points that close the
/// head, so both renderers draw the same triangle.
pub fn arrow_head(from: Point, to: Point, width: f32) -> (Point, Point) {
    let (dx, dy) = (to.x - from.x, to.y - from.y);
    let len = (dx * dx + dy * dy).sqrt().max(1.0);
    let (ux, uy) = (dx / len, dy / len);
    // Long enough to read at a glance, short enough not to swallow a small arrow.
    let head = (width * 4.0).min(len * 0.4);
    let (bx, by) = (to.x - ux * head, to.y - uy * head);
    let (px, py) = (-uy * head * 0.5, ux * head * 0.5);

    (Point::new(bx + px, by + py), Point::new(bx - px, by - py))
}

/// Draw the objects into a captured image.
///
/// `bounds` is the piece of the desktop the image covers, in the same coordinates the
/// objects are kept in; the image may be any pixel size, so everything is scaled by what
/// the two say about each other. Drawn with tiny-skia rather than by hand because the
/// screen preview goes through the same rasteriser - a mark that lands one pixel off in
/// the file is worse than no preview at all.
pub fn bake(image: &mut image::RgbaImage, objects: &[Object], bounds: crate::screenshot::Rect) {
    use tiny_skia::{LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke as SkStroke, Transform};

    if objects.is_empty() {
        return;
    }

    let (width, height) = (image.width(), image.height());
    let Some(mut pixmap) = Pixmap::new(width, height) else {
        tracing::error!("no pixmap for {width}x{height}: annotations not drawn");
        return;
    };

    // Logical units to image pixels. A monitor at 200% gives an image twice the logical
    // size, and a stroke has to grow with it or it comes out hairline in the file.
    let sx = width as f32 / (bounds.right - bounds.left).max(1) as f32;
    let sy = height as f32 / (bounds.bottom - bounds.top).max(1) as f32;
    let scale = (sx + sy) / 2.0;
    let at = |p: cosmic::iced::Point| {
        (
            (p.x - bounds.left as f32) * sx,
            (p.y - bounds.top as f32) * sy,
        )
    };

    for object in objects {
        let mut builder = PathBuilder::new();
        let stroke = match object {
            Object::Path { points, stroke } => {
                let Some((first, rest)) = points.split_first() else {
                    continue;
                };
                let (x, y) = at(*first);
                builder.move_to(x, y);
                for point in rest {
                    let (x, y) = at(*point);
                    builder.line_to(x, y);
                }
                *stroke
            }
            Object::Line { from, to, stroke } => {
                let (x, y) = at(*from);
                builder.move_to(x, y);
                let (x, y) = at(*to);
                builder.line_to(x, y);
                *stroke
            }
            Object::Arrow { from, to, stroke } => {
                let (left, right) = arrow_head(*from, *to, stroke.width);
                let (x, y) = at(*from);
                builder.move_to(x, y);
                let (x, y) = at(*to);
                builder.line_to(x, y);
                let (lx, ly) = at(left);
                builder.move_to(lx, ly);
                let (x, y) = at(*to);
                builder.line_to(x, y);
                let (rx, ry) = at(right);
                builder.line_to(rx, ry);
                *stroke
            }
            Object::Rectangle { from, to, stroke } => {
                let (x0, y0) = at(cosmic::iced::Point::new(from.x.min(to.x), from.y.min(to.y)));
                let (x1, y1) = at(cosmic::iced::Point::new(from.x.max(to.x), from.y.max(to.y)));
                builder.move_to(x0, y0);
                builder.line_to(x1, y0);
                builder.line_to(x1, y1);
                builder.line_to(x0, y1);
                builder.close();
                *stroke
            }
        };

        let Some(path) = builder.finish() else {
            continue;
        };

        let mut paint = Paint::default();
        paint.anti_alias = true;
        paint.set_color_rgba8(
            (stroke.color.r * 255.0) as u8,
            (stroke.color.g * 255.0) as u8,
            (stroke.color.b * 255.0) as u8,
            (stroke.color.a * 255.0) as u8,
        );

        let sk_stroke = SkStroke {
            width: stroke.width * scale,
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..SkStroke::default()
        };

        pixmap.stroke_path(&path, &paint, &sk_stroke, Transform::identity(), None);
    }

    // tiny-skia works premultiplied and the captured frame does not, so the marks are
    // composited by hand rather than copied over.
    for (pixel, drawn) in image.pixels_mut().zip(pixmap.pixels()) {
        let a = drawn.alpha() as f32 / 255.0;
        if a <= 0.0 {
            continue;
        }
        let demul = |c: u8| if a > 0.0 { c as f32 / a } else { 0.0 };
        for (i, c) in [
            demul(drawn.red()),
            demul(drawn.green()),
            demul(drawn.blue()),
        ]
        .into_iter()
        .enumerate()
        {
            pixel.0[i] = (c * a + pixel.0[i] as f32 * (1.0 - a)).clamp(0.0, 255.0) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::Color;

    fn editor_with(tool: Tool) -> Editor {
        Editor {
            tool: Some(tool),
            ..Editor::default()
        }
    }

    #[test]
    fn a_press_that_never_moved_leaves_nothing() {
        let mut editor = editor_with(Tool::Line);
        editor.press(Point::new(10.0, 10.0));
        editor.release();
        assert!(editor.is_empty(), "a missed press is not a mark");
    }

    #[test]
    fn undo_and_redo_walk_the_same_line() {
        let mut editor = editor_with(Tool::Line);
        editor.press(Point::new(0.0, 0.0));
        editor.drag(Point::new(50.0, 50.0));
        editor.release();
        assert_eq!(editor.objects().count(), 1);

        editor.undo();
        assert!(editor.is_empty());
        editor.redo();
        assert_eq!(editor.objects().count(), 1);
    }

    #[test]
    fn a_new_mark_ends_the_redo_trail() {
        let mut editor = editor_with(Tool::Line);
        editor.press(Point::new(0.0, 0.0));
        editor.drag(Point::new(50.0, 50.0));
        editor.release();
        editor.undo();

        editor.press(Point::new(10.0, 0.0));
        editor.drag(Point::new(60.0, 50.0));
        editor.release();
        editor.redo();

        assert_eq!(
            editor.objects().count(),
            1,
            "redo must not resurrect a history that no longer happened"
        );
    }

    #[test]
    fn with_no_tool_the_pointer_draws_nothing() {
        let mut editor = Editor::default();
        editor.press(Point::new(0.0, 0.0));
        editor.drag(Point::new(50.0, 50.0));
        editor.release();
        assert!(
            editor.is_empty(),
            "no tool means the selection gets the press"
        );
    }

    #[test]
    fn baking_puts_the_mark_where_the_preview_had_it() {
        // A 100x100 image over a 100x100 piece of desktop at (0, 0): one logical unit is
        // one pixel, so a horizontal line at y=50 has to land on row 50 and nowhere else.
        let mut image = image::RgbaImage::from_pixel(100, 100, image::Rgba([0, 0, 0, 255]));
        let objects = vec![Object::Line {
            from: Point::new(10.0, 50.0),
            to: Point::new(90.0, 50.0),
            stroke: Stroke {
                width: 2.0,
                color: Color::from_rgb(1.0, 0.0, 0.0),
            },
        }];

        bake(
            &mut image,
            &objects,
            crate::screenshot::Rect {
                left: 0,
                top: 0,
                right: 100,
                bottom: 100,
            },
        );

        assert!(
            image.get_pixel(50, 50).0[0] > 128,
            "the line is not on its row"
        );
        assert_eq!(
            image.get_pixel(50, 10).0,
            [0, 0, 0, 255],
            "nothing may be drawn away from the line"
        );
    }
}
