// Textured-quad pipeline for external pixel surfaces (browser frames, video).
//
// Matches the cell painter's conventions: gamma-space color (the texture is
// sampled without sRGB conversion) and premultiplied-alpha output for the
// One / OneMinusSrcAlpha blend.

struct PixelGlobals {
	// Destination rect in physical px: x, y, w, h (top-left origin).
	dst: vec4<f32>,
	// Render-target size in physical px.
	viewport: vec2<f32>,
	// Overall opacity multiplier.
	opacity: f32,
	_pad: f32,
}

@group(0) @binding(0) var<uniform> globals: PixelGlobals;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct VsOut {
	@builtin(position) pos: vec4<f32>,
	@location(0) uv: vec2<f32>,
}

@vertex
fn vs_pixel(@builtin(vertex_index) index: u32) -> VsOut {
	// Triangle-strip corners (0,0) (1,0) (0,1) (1,1).
	let corner = vec2<f32>(f32(index & 1u), f32(index >> 1u));
	let px = globals.dst.xy + corner * globals.dst.zw;
	let ndc = vec2<f32>(
		px.x / globals.viewport.x * 2.0 - 1.0,
		1.0 - px.y / globals.viewport.y * 2.0,
	);
	var out: VsOut;
	out.pos = vec4<f32>(ndc, 0.0, 1.0);
	out.uv = corner;
	return out;
}

@fragment
fn fs_pixel(in: VsOut) -> @location(0) vec4<f32> {
	let c = textureSample(tex, samp, in.uv);
	let a = c.a * globals.opacity;
	// Straight-alpha source premultiplied here for the shared blend state.
	return vec4<f32>(c.rgb * a, a);
}
