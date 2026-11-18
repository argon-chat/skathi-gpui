use super::WgpuRendererCore;
use super::backdrop::{
    TargetTexture, capture_validation, create_target_texture, pipeline_layout, render_pipeline,
    shader_module,
};
use anyhow::Result;
use bytemuck::{Pod, Zeroable};
use gpui::HdrOutput;

/// The surface and scene format of an HDR window.
pub(super) const HDR_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

const HDR_OUTPUT_SHADER: &str = include_str!("../hdr_output.wgsl");

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct HdrParams {
    sdr_white: f32,
    premultiplied: u32,
    pad: [u32; 2],
}

/// The scene blends into a half-float target in sRGB-encoded space, as on an
/// 8-bit surface, and one pass linearises it into the surface.
pub(super) struct HdrState {
    output: HdrOutput,
    layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    params: wgpu::Buffer,
    scene: Option<TargetTexture>,
}

impl HdrState {
    fn new(device: &wgpu::Device, output: HdrOutput) -> Result<Self> {
        let fragment = wgpu::ShaderStages::FRAGMENT;
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("hdr_output_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: fragment,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: fragment,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
            ],
        });
        let pipeline = capture_validation(device, || {
            let module = shader_module(device, "hdr_output", HDR_OUTPUT_SHADER);
            render_pipeline(
                device,
                "hdr_output",
                &pipeline_layout(device, "hdr_output_layout", &[&layout]),
                (&module, "vs_hdr_output"),
                (&module, "fs_hdr_output"),
                HDR_FORMAT,
                None,
            )
        })?;
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("hdr_output_params"),
            size: size_of::<HdrParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Ok(Self {
            output,
            layout,
            pipeline,
            params,
            scene: None,
        })
    }
}

impl WgpuRendererCore {
    /// Switches the frame between the surface's own format and half float;
    /// `false` leaves it as it was.
    pub(super) fn set_hdr_output(
        &mut self,
        output: Option<HdrOutput>,
        alpha_mode: wgpu::CompositeAlphaMode,
    ) -> bool {
        match (output, &mut self.hdr) {
            (Some(output), Some(state)) => {
                state.output = output;
                return true;
            }
            (None, None) => return true,
            _ => {}
        }
        self.hdr = match output {
            Some(output) => match HdrState::new(&self.resources.device, output) {
                Ok(state) => Some(Box::new(state)),
                Err(error) => {
                    log::error!("HDR output is unavailable: {error:#}");
                    return false;
                }
            },
            None => None,
        };
        self.target_format = if self.hdr.is_some() {
            HDR_FORMAT
        } else {
            self.sdr_format
        };
        self.rebuild_pipelines(self.target_format, alpha_mode);
        self.resources.backdrop = Default::default();
        self.resources.vn_layers = Default::default();
        true
    }

    /// The target an HDR frame draws its scene into, sized like the output.
    pub(super) fn hdr_scene(&mut self, width: u32, height: u32) -> Option<TargetTexture> {
        let state = self.hdr.as_mut()?;
        if state
            .scene
            .as_ref()
            .is_none_or(|scene| scene.texture.width() != width || scene.texture.height() != height)
        {
            state.scene = Some(create_target_texture(
                &self.resources.device,
                "hdr_scene",
                HDR_FORMAT,
                width,
                height,
                wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
            ));
        }
        state.scene.clone()
    }

    pub(super) fn draw_hdr_output(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        scene: &wgpu::TextureView,
        output: &wgpu::TextureView,
        premultiplied: bool,
    ) {
        let Some(state) = &self.hdr else {
            return;
        };
        let device = &self.resources.device;
        self.resources.queue.write_buffer(
            &state.params,
            0,
            bytemuck::bytes_of(&HdrParams {
                sdr_white: state.output.sdr_white,
                premultiplied: premultiplied as u32,
                pad: [0; 2],
            }),
        );
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("hdr_output"),
            layout: &state.layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: state.params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(scene),
                },
            ],
        });
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("hdr_output"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: output,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: None,
            ..Default::default()
        });
        pass.set_pipeline(&state.pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}
