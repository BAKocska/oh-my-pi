//! External pixel surfaces composited as textured quads.
//!
//! The cell painter draws everything the compositor produces from retained
//! `Frame` grids; content rendered *outside* that pipeline — an embedded
//! browser's frame stream, video, any offscreen RGBA producer — enters here.
//! A [`PixelSurface`] owns one GPU texture kept current with
//! [`PixelSurface::upload`]; a [`PixelPainter`] draws any number of surfaces
//! as quads into an existing render target, before or after the cell pass.
//!
//! Color conventions match the cell painter: the texture is sampled without
//! sRGB conversion (the window target is non-sRGB, gamma-space), and the
//! fragment output is premultiplied for the shared
//! `One / OneMinusSrcAlpha` blend.

use crate::gpu::Gpu;

/// Uniform block layout mirrored by `PixelGlobals` in pixels.wgsl.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PixelGlobals {
	/// Destination rect `x, y, w, h` in physical px.
	dst:      [f32; 4],
	/// Render-target size in physical px.
	viewport: [f32; 2],
	/// Overall opacity multiplier.
	opacity:  f32,
	_pad:     f32,
}

/// One quad to draw in a [`PixelPainter::draw`] pass.
pub struct PixelDraw<'s> {
	/// The surface to sample.
	pub surface: &'s PixelSurface,
	/// Destination rect `x, y, w, h` in physical px (top-left origin).
	pub dst:     [f32; 4],
	/// Opacity multiplier in `0.0..=1.0`.
	pub opacity: f32,
}

/// Pipeline and shared sampler for drawing [`PixelSurface`]s.
pub struct PixelPainter {
	pipeline: wgpu::RenderPipeline,
	layout:   wgpu::BindGroupLayout,
	sampler:  wgpu::Sampler,
}

impl PixelPainter {
	/// Builds the quad pipeline for targets of `format`.
	pub fn new(gpu: &Gpu, format: wgpu::TextureFormat) -> Self {
		let device = &gpu.device;
		let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
			label:  Some("pixel-quad"),
			source: wgpu::ShaderSource::Wgsl(include_str!("pixels.wgsl").into()),
		});
		let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
			label:   Some("pixel-surface"),
			entries: &[
				wgpu::BindGroupLayoutEntry {
					binding:    0,
					visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
					ty:         wgpu::BindingType::Buffer {
						ty:                 wgpu::BufferBindingType::Uniform,
						has_dynamic_offset: false,
						min_binding_size:   None,
					},
					count:      None,
				},
				wgpu::BindGroupLayoutEntry {
					binding:    1,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty:         wgpu::BindingType::Texture {
						sample_type:    wgpu::TextureSampleType::Float { filterable: true },
						view_dimension: wgpu::TextureViewDimension::D2,
						multisampled:   false,
					},
					count:      None,
				},
				wgpu::BindGroupLayoutEntry {
					binding:    2,
					visibility: wgpu::ShaderStages::FRAGMENT,
					ty:         wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
					count:      None,
				},
			],
		});
		let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
			label:              Some("pixel-quad"),
			bind_group_layouts: &[Some(&layout)],
			immediate_size:     0,
		});
		// The same premultiplied blend the cell painter uses.
		let blend = wgpu::BlendState {
			color: wgpu::BlendComponent {
				src_factor: wgpu::BlendFactor::One,
				dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
				operation:  wgpu::BlendOperation::Add,
			},
			alpha: wgpu::BlendComponent {
				src_factor: wgpu::BlendFactor::One,
				dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
				operation:  wgpu::BlendOperation::Add,
			},
		};
		let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
			label:          Some("pixel-quad"),
			layout:         Some(&pipeline_layout),
			vertex:         wgpu::VertexState {
				module:              &shader,
				entry_point:         Some("vs_pixel"),
				compilation_options: Default::default(),
				buffers:             &[],
			},
			fragment:       Some(wgpu::FragmentState {
				module:              &shader,
				entry_point:         Some("fs_pixel"),
				compilation_options: Default::default(),
				targets:             &[Some(wgpu::ColorTargetState {
					format,
					blend: Some(blend),
					write_mask: wgpu::ColorWrites::ALL,
				})],
			}),
			primitive:      wgpu::PrimitiveState {
				topology: wgpu::PrimitiveTopology::TriangleStrip,
				..Default::default()
			},
			depth_stencil:  None,
			multisample:    wgpu::MultisampleState::default(),
			multiview_mask: None,
			cache:          None,
		});
		let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
			label: Some("pixel-surface"),
			mag_filter: wgpu::FilterMode::Linear,
			min_filter: wgpu::FilterMode::Linear,
			..Default::default()
		});
		Self { pipeline, layout, sampler }
	}

	/// Creates an empty (fully transparent) surface; size it with
	/// [`PixelSurface::upload`].
	pub fn surface(&self, gpu: &Gpu) -> PixelSurface {
		PixelSurface::new(gpu, self, 1, 1)
	}

	/// Draws `draws` in order into `target` in one render pass.
	///
	/// `load` keeps existing target content (`Load`) when compositing over a
	/// prior pass, or clears first (`Clear`) when the surfaces are the whole
	/// frame.
	pub fn draw(
		&self,
		gpu: &Gpu,
		target: &wgpu::TextureView,
		viewport: (u32, u32),
		load: wgpu::LoadOp<wgpu::Color>,
		draws: &[PixelDraw<'_>],
	) {
		for draw in draws {
			let globals = PixelGlobals {
				dst:      draw.dst,
				viewport: [viewport.0 as f32, viewport.1 as f32],
				opacity:  draw.opacity,
				_pad:     0.0,
			};
			gpu.queue
				.write_buffer(&draw.surface.globals, 0, bytemuck::bytes_of(&globals));
		}
		let mut encoder = gpu
			.device
			.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("pixel-quads") });
		{
			let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
				label:                    Some("pixel-pass"),
				color_attachments:        &[Some(wgpu::RenderPassColorAttachment {
					view:           target,
					depth_slice:    None,
					resolve_target: None,
					ops:            wgpu::Operations { load, store: wgpu::StoreOp::Store },
				})],
				depth_stencil_attachment: None,
				timestamp_writes:         None,
				occlusion_query_set:      None,
				multiview_mask:           None,
			});
			pass.set_pipeline(&self.pipeline);
			for draw in draws {
				pass.set_bind_group(0, &draw.surface.bind, &[]);
				pass.draw(0..4, 0..1);
			}
		}
		gpu.queue.submit([encoder.finish()]);
	}
}

/// One GPU texture kept current from CPU-side RGBA8 frames.
pub struct PixelSurface {
	texture: wgpu::Texture,
	bind:    wgpu::BindGroup,
	globals: wgpu::Buffer,
	width:   u32,
	height:  u32,
}

impl PixelSurface {
	fn new(gpu: &Gpu, painter: &PixelPainter, width: u32, height: u32) -> Self {
		let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
			label:           Some("pixel-surface"),
			size:            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
			mip_level_count: 1,
			sample_count:    1,
			dimension:       wgpu::TextureDimension::D2,
			// Non-sRGB on purpose: sampled values stay gamma-encoded to match
			// the painter's gamma-space render target.
			format:          wgpu::TextureFormat::Rgba8Unorm,
			usage:           wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
			view_formats:    &[],
		});
		let globals = gpu.device.create_buffer(&wgpu::BufferDescriptor {
			label:              Some("pixel-globals"),
			size:               size_of::<PixelGlobals>() as u64,
			usage:              wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
			mapped_at_creation: false,
		});
		let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
		let bind = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
			label:   Some("pixel-surface"),
			layout:  &painter.layout,
			entries: &[
				wgpu::BindGroupEntry { binding: 0, resource: globals.as_entire_binding() },
				wgpu::BindGroupEntry {
					binding:  1,
					resource: wgpu::BindingResource::TextureView(&view),
				},
				wgpu::BindGroupEntry {
					binding:  2,
					resource: wgpu::BindingResource::Sampler(&painter.sampler),
				},
			],
		});
		Self { texture, bind, globals, width, height }
	}

	/// Uploads a tightly packed RGBA8 frame, recreating the texture when the
	/// dimensions change.
	///
	/// `damage` (`[x, y, w, h]` in pixels) limits the upload to that region;
	/// `rgba` must still be the complete frame (the region is addressed via
	/// row stride). A region upload assumes the texture already holds the
	/// previous frame, so when frames are skipped the caller passes the
	/// union of the skipped frames' damage. `None`, a fresh texture, or a
	/// size change upload the full frame.
	///
	/// # Panics
	///
	/// When `rgba` is not exactly `width * height * 4` bytes, or `damage`
	/// exceeds the frame bounds.
	pub fn upload(
		&mut self,
		gpu: &Gpu,
		painter: &PixelPainter,
		width: u32,
		height: u32,
		rgba: &[u8],
		damage: Option<[u32; 4]>,
	) {
		assert_eq!(
			rgba.len() as u64,
			u64::from(width) * u64::from(height) * 4,
			"frame size mismatch"
		);
		if width == 0 || height == 0 {
			return;
		}
		let region = if (width, height) == (self.width, self.height) {
			damage.unwrap_or([0, 0, width, height])
		} else {
			*self = Self::new(gpu, painter, width, height);
			[0, 0, width, height]
		};
		let [x, y, w, h] = region;
		assert!(x + w <= width && y + h <= height, "damage out of bounds");
		if w == 0 || h == 0 {
			return;
		}
		// Tight rows go straight to `write_texture`: wgpu stages internally
		// and repacks unaligned rows itself, so pre-padding here would only
		// add a redundant CPU copy. A damage region is addressed inside the
		// full frame via `offset` + the full-row stride.
		gpu.queue.write_texture(
			wgpu::TexelCopyTextureInfo {
				texture:   &self.texture,
				mip_level: 0,
				origin:    wgpu::Origin3d { x, y, z: 0 },
				aspect:    wgpu::TextureAspect::All,
			},
			rgba,
			wgpu::TexelCopyBufferLayout {
				offset:         (u64::from(y) * u64::from(width) + u64::from(x)) * 4,
				bytes_per_row:  Some(width * 4),
				rows_per_image: Some(h),
			},
			wgpu::Extent3d {
				width:                 w,
				height:                h,
				depth_or_array_layers: 1,
			},
		);
	}

	/// Current texture dimensions in pixels.
	pub const fn size(&self) -> (u32, u32) {
		(self.width, self.height)
	}
}
