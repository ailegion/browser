//! Transform matrices (CSS Transforms 1 §9, 2 §13): the functions of a
//! `transform` list (and the individual `translate`, `rotate`, `scale`)
//! composed into one 4×4 matrix, serialized as the CSSOM resolves it
//! (`matrix()` for a 2D one, `matrix3d()` otherwise) and flattened to
//! the 2D affine the painter can apply. Without `perspective` the
//! flattening is the orthographic projection browsers use for a flat
//! transform-style, so 3D rotations and translations come out exactly;
//! a perspective component is dropped (roadmap 4a block 6 records it).

use crate::computed::ComputedStyle;
use crate::properties::ComputedTransformFunction;
use crate::values::{ComputedLp, css_number};

/// A column-major 4×4 matrix as CSS Transforms writes `matrix3d()`:
/// `m[column * 4 + row]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Matrix4 {
    pub m: [f64; 16],
}

impl Matrix4 {
    pub const IDENTITY: Matrix4 = Matrix4 {
        m: [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0],
    };

    fn at(&self, row: usize, col: usize) -> f64 {
        self.m[col * 4 + row]
    }

    /// `self × other` (the other applied first to a point, as a later
    /// function in a transform list is).
    pub fn mul(&self, other: &Matrix4) -> Matrix4 {
        let mut m = [0.0; 16];
        for col in 0..4 {
            for row in 0..4 {
                m[col * 4 + row] = (0..4).map(|k| self.at(row, k) * other.at(k, col)).sum();
            }
        }
        Matrix4 { m }
    }

    pub fn translate(x: f64, y: f64, z: f64) -> Matrix4 {
        let mut t = Matrix4::IDENTITY;
        t.m[12] = x;
        t.m[13] = y;
        t.m[14] = z;
        t
    }

    pub fn scale(x: f64, y: f64, z: f64) -> Matrix4 {
        let mut s = Matrix4::IDENTITY;
        s.m[0] = x;
        s.m[5] = y;
        s.m[10] = z;
        s
    }

    /// Rotation by `deg` about the unit axis (x, y, z) (§13.1).
    pub fn rotate(x: f64, y: f64, z: f64, deg: f64) -> Matrix4 {
        let len = (x * x + y * y + z * z).sqrt();
        if len == 0.0 {
            return Matrix4::IDENTITY;
        }
        let (x, y, z) = (x / len, y / len, z / len);
        let (s, c) = deg.to_radians().sin_cos();
        let t = 1.0 - c;
        Matrix4 {
            m: [
                t * x * x + c,
                t * x * y + s * z,
                t * x * z - s * y,
                0.0,
                t * x * y - s * z,
                t * y * y + c,
                t * y * z + s * x,
                0.0,
                t * x * z + s * y,
                t * y * z - s * x,
                t * z * z + c,
                0.0,
                0.0,
                0.0,
                0.0,
                1.0,
            ],
        }
    }

    pub fn skew(x_deg: f64, y_deg: f64) -> Matrix4 {
        let mut s = Matrix4::IDENTITY;
        s.m[4] = x_deg.to_radians().tan();
        s.m[1] = y_deg.to_radians().tan();
        s
    }

    pub fn from_2d(a: [f64; 6]) -> Matrix4 {
        let mut m = Matrix4::IDENTITY;
        m.m[0] = a[0];
        m.m[1] = a[1];
        m.m[4] = a[2];
        m.m[5] = a[3];
        m.m[12] = a[4];
        m.m[13] = a[5];
        m
    }

    /// Whether the matrix is a 2D one (§9: no z, no perspective).
    pub fn is_2d(&self) -> bool {
        let m = &self.m;
        m[2] == 0.0
            && m[3] == 0.0
            && m[6] == 0.0
            && m[7] == 0.0
            && m[8] == 0.0
            && m[9] == 0.0
            && m[10] == 1.0
            && m[11] == 0.0
            && m[14] == 0.0
            && m[15] == 1.0
    }

    pub fn is_identity(&self) -> bool {
        *self == Matrix4::IDENTITY
    }

    /// The 2D affine `[a, b, c, d, e, f]` the matrix paints as: its x and
    /// y rows, the z axis projected away (orthographic).
    pub fn to_affine(&self) -> [f64; 6] {
        let m = &self.m;
        [m[0], m[1], m[4], m[5], m[12], m[13]]
    }

    /// Whether the box shows its back (`backface-visibility`): the z axis
    /// turned to face away, as Blink tests it.
    pub fn backface_turned(&self) -> bool {
        self.at(2, 2) < 0.0
    }

    /// The CSSOM serialization: `matrix(a, b, c, d, e, f)` or
    /// `matrix3d(16 values)`.
    pub fn to_css(&self) -> String {
        if self.is_2d() {
            let a = self.to_affine();
            format!(
                "matrix({}, {}, {}, {}, {}, {})",
                css_number(a[0] as f32),
                css_number(a[1] as f32),
                css_number(a[2] as f32),
                css_number(a[3] as f32),
                css_number(a[4] as f32),
                css_number(a[5] as f32)
            )
        } else {
            let parts: Vec<String> = self.m.iter().map(|v| css_number(*v as f32)).collect();
            format!("matrix3d({})", parts.join(", "))
        }
    }
}

/// The matrix of one computed function for a reference box `width` ×
/// `height` px (percentages of translations resolve against it).
pub fn function_matrix(f: &ComputedTransformFunction, width: f32, height: f32) -> Matrix4 {
    let lp = |v: ComputedLp, basis: f32| f64::from(v.resolve(basis));
    match f {
        ComputedTransformFunction::Matrix(a) => Matrix4::from_2d(a.map(f64::from)),
        ComputedTransformFunction::Matrix3d(a) => Matrix4 { m: a.map(f64::from) },
        ComputedTransformFunction::Translate(x, y, z) => Matrix4::translate(lp(*x, width), lp(*y, height), f64::from(*z)),
        ComputedTransformFunction::Scale(x, y, z) => Matrix4::scale(f64::from(*x), f64::from(*y), f64::from(*z)),
        ComputedTransformFunction::Rotate(x, y, z, deg) => Matrix4::rotate(f64::from(*x), f64::from(*y), f64::from(*z), f64::from(*deg)),
        ComputedTransformFunction::Skew(x, y) => Matrix4::skew(f64::from(*x), f64::from(*y)),
        // Perspective: not renderable as an affine; the identity (see
        // the module note).
        ComputedTransformFunction::Perspective(_) => Matrix4::IDENTITY,
    }
}

/// The full transform of a box (CSS Transforms 2 §6): `translate`, then
/// `rotate`, then `scale`, then the `transform` list, all about the
/// origin; `None` when nothing transforms. The matrix is about the
/// reference box's top-left; the caller applies `transform-origin`.
pub fn transform_matrix(style: &ComputedStyle, width: f32, height: f32) -> Option<Matrix4> {
    let mut m = Matrix4::IDENTITY;
    let mut any = false;
    if let Some((x, y, z)) = style.translate {
        m = m.mul(&Matrix4::translate(f64::from(x.resolve(width)), f64::from(y.resolve(height)), f64::from(z)));
        any = true;
    }
    if let Some((x, y, z, deg)) = style.rotate {
        m = m.mul(&Matrix4::rotate(f64::from(x), f64::from(y), f64::from(z), f64::from(deg)));
        any = true;
    }
    if let Some((x, y, z)) = style.scale {
        m = m.mul(&Matrix4::scale(f64::from(x), f64::from(y), f64::from(z)));
        any = true;
    }
    for f in &style.transform {
        m = m.mul(&function_matrix(f, width, height));
        any = true;
    }
    any.then_some(m)
}

/// A box's transform about its `transform-origin`, relative to the
/// reference box's top-left: `T(origin) × M × T(-origin)`; `None` when
/// nothing transforms.
pub fn box_matrix(style: &ComputedStyle, width: f32, height: f32) -> Option<Matrix4> {
    let m = transform_matrix(style, width, height)?;
    let (ox, oy, oz) = style.transform_origin;
    let ox = f64::from(ox.resolve(width));
    let oy = f64::from(oy.resolve(height));
    Some(Matrix4::translate(ox, oy, f64::from(oz)).mul(&m).mul(&Matrix4::translate(-ox, -oy, -f64::from(oz))))
}

/// The 2D affine of `box_matrix`, flattened, and whether the box shows
/// its back.
pub fn box_affine(style: &ComputedStyle, width: f32, height: f32) -> Option<([f64; 6], bool)> {
    let about = box_matrix(style, width, height)?;
    Some((about.to_affine(), about.backface_turned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrices_compose_and_serialize() {
        let r = Matrix4::rotate(0.0, 0.0, 1.0, 90.0);
        let a = r.to_affine();
        assert!((a[0]).abs() < 1e-9 && (a[1] - 1.0).abs() < 1e-9 && (a[2] + 1.0).abs() < 1e-9 && (a[3]).abs() < 1e-9);
        assert_eq!(Matrix4::translate(10.0, 20.0, 0.0).to_css(), "matrix(1, 0, 0, 1, 10, 20)");
        let ry = Matrix4::rotate(0.0, 1.0, 0.0, 180.0);
        assert!(!ry.is_2d() && ry.backface_turned());
        assert!(ry.to_css().starts_with("matrix3d("));
        // translateZ alone paints as the identity (orthographic).
        assert_eq!(Matrix4::translate(0.0, 0.0, 50.0).to_affine(), [1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
        // A translate then a scale: the scale applies first to points.
        let m = Matrix4::translate(10.0, 0.0, 0.0).mul(&Matrix4::scale(2.0, 2.0, 1.0));
        assert_eq!(m.to_affine(), [2.0, 0.0, 0.0, 2.0, 10.0, 0.0]);
    }
}
