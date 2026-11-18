//! A scene of CSS `corner-shape` primitives and the pixels each backend must paint for it.

use crate::{
    BackdropBlurRect, BorderStyle, Bounds, ContentMask, CornerShape, CornerShapes, Corners, Edges,
    Hsla, Quad, ScaledPixels, Scene, Shadow, TransformationMatrix, point, rgb, size,
};

/// The size the scene is laid out for.
pub const SIZE: (u32, u32) = (200, 104);

const RADIUS: f32 = 12.;
const SIDE: f32 = 28.;

fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<ScaledPixels> {
    Bounds::new(
        point(ScaledPixels(x), ScaledPixels(y)),
        size(ScaledPixels(width), ScaledPixels(height)),
    )
}

fn all(shape: CornerShape) -> Corners<CornerShape> {
    Corners {
        top_left: shape,
        top_right: shape,
        bottom_right: shape,
        bottom_left: shape,
    }
}

fn shaped(shape: CornerShape) -> CornerShapes {
    CornerShapes::new(&all(shape))
}

fn page() -> ContentMask<ScaledPixels> {
    ContentMask {
        bounds: bounds(0., 0., SIZE.0 as f32, SIZE.1 as f32),
        ..Default::default()
    }
}

fn quad(x: f32, y: f32, shape: CornerShape) -> Quad {
    Quad {
        bounds: bounds(x, y, SIDE, SIDE),
        background: rgb(0xffffff).into(),
        corner_radii: Corners::all(ScaledPixels(RADIUS)),
        content_mask: page(),
        transformation: TransformationMatrix::unit(),
        corner_shapes: shaped(shape),
        ..Default::default()
    }
}

const KEYWORDS: [CornerShape; 6] = [
    CornerShape::ROUND,
    CornerShape::SQUIRCLE,
    CornerShape::BEVEL,
    CornerShape::SCOOP,
    CornerShape::NOTCH,
    CornerShape::SQUARE,
];

/// Row one: a white quad per keyword. Row two: a bevelled border, a quad clipped by a
/// bevelled mask with insets, a dashed scoop border, drop and inset shadows and a blur.
pub fn scene() -> Scene {
    let mut scene = Scene::default();
    scene.insert_primitive(Quad {
        bounds: page().bounds,
        background: rgb(0x000000).into(),
        content_mask: page(),
        transformation: TransformationMatrix::unit(),
        ..Default::default()
    });
    for (i, shape) in KEYWORDS.into_iter().enumerate() {
        scene.insert_primitive(quad(4. + i as f32 * 32., 4., shape));
    }

    let row = 40.;
    scene.insert_primitive(Quad {
        border_color: rgb(0xff0000).into(),
        border_widths: Edges::all(ScaledPixels(4.)),
        ..quad(4., row, CornerShape::BEVEL)
    });
    let insets = Corners {
        top_left: (4., 4.),
        top_right: (4., 4.),
        bottom_right: (4., 4.),
        bottom_left: (4., 4.),
    };
    scene.insert_primitive(Quad {
        bounds: bounds(36., row, SIDE, SIDE),
        background: rgb(0x00ff00).into(),
        content_mask: ContentMask {
            bounds: bounds(36., row, SIDE, SIDE),
            corner_radii: Corners::all(ScaledPixels(RADIUS)),
            corner_shapes: shaped(CornerShape::BEVEL).with_insets(insets),
        },
        transformation: TransformationMatrix::unit(),
        ..Default::default()
    });
    scene.insert_primitive(Quad {
        border_color: rgb(0x40a0ff).into(),
        border_widths: Edges::all(ScaledPixels(3.)),
        border_style: BorderStyle::Dashed,
        ..quad(68., row, CornerShape::SCOOP)
    });
    let element = bounds(100., row + 34., SIDE, SIDE);
    let shadow = |area, shape_bounds, inset, shape| Shadow {
        order: 0,
        blur_radius: ScaledPixels(3.),
        bounds: area,
        corner_radii: Corners::all(ScaledPixels(RADIUS)),
        content_mask: page(),
        color: Hsla {
            a: 0.9,
            ..Hsla::from(rgb(0xffd000))
        },
        shape_bounds,
        shape_corner_radii: Corners::all(ScaledPixels(RADIUS - 2.)),
        inset,
        pad: 0,
        transformation: TransformationMatrix::unit(),
        element_bounds: element,
        corner_shapes: shaped(shape),
    };
    scene.insert_primitive(shadow(
        bounds(92., row + 30., SIDE + 16., 36.),
        bounds(102., row + 40., SIDE, 22.),
        0,
        CornerShape::SQUIRCLE,
    ));
    scene.insert_primitive(Shadow {
        element_bounds: bounds(132., row + 34., SIDE, SIDE),
        ..shadow(
            bounds(132., row + 34., SIDE, SIDE),
            bounds(136., row + 38., SIDE - 8., SIDE - 8.),
            1,
            CornerShape::NOTCH,
        )
    });
    scene.insert_primitive(BackdropBlurRect {
        bounds: bounds(164., row, SIDE, SIDE + 30.),
        content_mask: page(),
        corner_radii: Corners::all(ScaledPixels(RADIUS)),
        blur_radius: ScaledPixels(4.),
        opacity: 1.,
        tint: Hsla {
            a: 0.6,
            ..Hsla::from(rgb(0x8080ff))
        },
        corner_shapes: shaped(CornerShape::BEVEL),
        ..Default::default()
    });
    scene.finish();
    scene
}

/// `(x, y, rgba, what)`: a pixel well clear of every edge, so any backend paints it exactly.
pub fn expectations() -> Vec<(u32, u32, [u8; 4], &'static str)> {
    const WHITE: [u8; 4] = [255, 255, 255, 255];
    const BLACK: [u8; 4] = [0, 0, 0, 255];
    let x = |i: u32| 4 + i * 32;
    vec![
        (x(0) + 1, 5, BLACK, "round cuts its corner"),
        (x(0) + 5, 9, WHITE, "round fills inside its arc"),
        (x(1) + 3, 7, WHITE, "squircle fills where round does not"),
        (x(1), 4, BLACK, "squircle still cuts the very corner"),
        (x(2) + 3, 7, BLACK, "bevel cuts a triangle"),
        (x(2) + 8, 11, WHITE, "bevel fills past its diagonal"),
        (x(3) + 6, 10, BLACK, "scoop carves a quarter circle"),
        (x(3) + 11, 15, WHITE, "scoop fills past its arc"),
        (x(4) + 9, 13, BLACK, "notch cuts a square"),
        (x(4) + 14, 17, WHITE, "notch fills past its square"),
        (x(4) + 9, 18, WHITE, "notch fills beside its square"),
        (x(5), 4, WHITE, "square fills its corner"),
        (
            11,
            47,
            [255, 0, 0, 255],
            "a bevelled border follows the diagonal",
        ),
        (15, 51, WHITE, "inside a bevelled border"),
        (6, 42, BLACK, "outside a bevelled border"),
        (37, 41, BLACK, "a bevelled mask cuts its corner"),
        (49, 53, [0, 255, 0, 255], "inside a bevelled mask"),
    ]
}
