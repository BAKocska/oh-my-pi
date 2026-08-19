//! Geometry shared by child surfaces.

/// A rectangle in logical points, relative to the parent window's top-left
/// corner (y grows downward, matching winit and web coordinates).
///
/// Hosts working in physical pixels divide by the window scale factor before
/// passing bounds here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
	/// Left edge offset from the parent's left edge.
	pub x:      f64,
	/// Top edge offset from the parent's top edge.
	pub y:      f64,
	/// Width in logical points.
	pub width:  f64,
	/// Height in logical points.
	pub height: f64,
}

impl Rect {
	/// A rectangle at `(x, y)` sized `width` x `height`, all in logical points.
	#[inline]
	pub const fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
		Self { x, y, width, height }
	}
}

impl Default for Rect {
	fn default() -> Self {
		Self::new(0.0, 0.0, 800.0, 600.0)
	}
}
