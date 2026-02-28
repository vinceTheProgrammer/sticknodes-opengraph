use anyhow::Result;
use glam::{Mat4, Vec3};
use image::{ImageBuffer, Rgba};
use snrs_render_core::CompiledFrame;
use wgpu::util::DeviceExt;

use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GpuVertex {
    position: [f32; 2],
    color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Uniforms {
    transform: [[f32; 4]; 4],
}

impl GpuVertex {
    fn desc<'a>() -> wgpu::VertexBufferLayout<'a> {
        use std::mem;
        wgpu::VertexBufferLayout {
            array_stride: mem::size_of::<GpuVertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &[
                // position
                wgpu::VertexAttribute {
                    offset: 0,
                    shader_location: 0,
                    format: wgpu::VertexFormat::Float32x2,
                },
                // color
                wgpu::VertexAttribute {
                    offset: mem::size_of::<[f32; 2]>() as u64,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x4,
                },
            ],
        }
    }
}

pub async fn render_to_png(frame: &CompiledFrame) -> Result<Vec<u8>> {
    let width: u32 = 1024;
    let height: u32 = 1024;

    let bounds = frame.bounds();

    let content_width = bounds.max.x - bounds.min.x;
    let content_height = bounds.max.y - bounds.min.y;

    let padding = 0.9;

    let scale_x = width as f32 / content_width;
    let scale_y = height as f32 / content_height;
    let mut scale = scale_x.min(scale_y) * padding;

    if content_width <= 0.0 || content_height <= 0.0 {
        scale = 1.0;
    }

    let center_x = (bounds.min.x + bounds.max.x) * 0.5;
    let center_y = (bounds.min.y + bounds.max.y) * 0.5;

    // Translate content center to origin
    let translate = Mat4::from_translation(Vec3::new(-center_x, -center_y, 0.0));

    // Scale to fit
    let scale_mat = Mat4::from_scale(Vec3::new(scale, scale, 1.0));

    // Convert to NDC
    let ndc = Mat4::from_scale(Vec3::new(
        2.0 / width as f32,
        2.0 / height as f32,
        1.0,
    ));

    // Final matrix
    let final_matrix = ndc * scale_mat * translate;

    // --- Instance / Adapter / Device ---
    let instance = wgpu::Instance::default();

    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions::default())
        .await
        .ok_or_else(|| anyhow::anyhow!("No suitable GPU adapters found"))?;

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor::default(), None)
        .await?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Shader"),
            source: wgpu::ShaderSource::Wgsl(r#"
                struct Uniforms {
                    transform: mat4x4<f32>,
                };
    
                @group(0) @binding(0)
                var<uniform> uniforms: Uniforms;
    
                struct VertexOut {
                    @builtin(position) position: vec4<f32>,
                    @location(0) color: vec4<f32>,
                };
    
                @vertex
                fn vs_main(
                    @location(0) position: vec2<f32>,
                    @location(1) color: vec4<f32>
                ) -> VertexOut {
                    var out: VertexOut;
    
                    let pos = vec4<f32>(position, 0.0, 1.0);
                    out.position = uniforms.transform * pos;
    
                    out.color = color;
                    return out;
                }
    
                @fragment
                fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
                    return in.color;
                }
            "#.into()),
        });

    // --- Render Target Texture ---
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("Render Texture"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });

    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    // --- Create Output Buffer (for readback) ---
    let bytes_per_pixel = 4u32;
    let unpadded_bytes_per_row = width * bytes_per_pixel;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded_bytes_per_row =
        ((unpadded_bytes_per_row + align - 1) / align) * align;

    let output_buffer_size =
        (padded_bytes_per_row * height) as wgpu::BufferAddress;

    let output_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("Output Buffer"),
        size: output_buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    // --- Create Command Encoder ---
    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("Render Encoder"),
        });
    
    let uniforms = Uniforms {
        transform: final_matrix.to_cols_array_2d(),
    };
    
    let uniform_buffer = device.create_buffer_init(
        &wgpu::util::BufferInitDescriptor {
            label: Some("Uniform Buffer"),
            contents: bytemuck::bytes_of(&uniforms),
            usage: wgpu::BufferUsages::UNIFORM,
        },
    );

    let bind_group_layout =
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Bind Group Layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("Bind Group"),
        layout: &bind_group_layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: uniform_buffer.as_entire_binding(),
        }],
    });

    let pipeline_layout = device.create_pipeline_layout(
        &wgpu::PipelineLayoutDescriptor {
            label: Some("Pipeline Layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        },
    );
    
    let pipeline = device.create_render_pipeline(
        &wgpu::RenderPipelineDescriptor {
            label: Some("Pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[GpuVertex::desc()],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8UnormSrgb,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
        },
    );

    // Convert to GPU format
    let gpu_vertices: Vec<GpuVertex> = frame.vertices.iter().map(|v| {
        GpuVertex {
            position: [v.position.x, v.position.y],
            color: v.color,
        }
    }).collect();

    let vertex_buffer = device.create_buffer_init(
        &wgpu::util::BufferInitDescriptor {
            label: Some("Vertex Buffer"),
            contents: bytemuck::cast_slice(&gpu_vertices),
            usage: wgpu::BufferUsages::VERTEX,
        },
    );

    let index_buffer = device.create_buffer_init(
        &wgpu::util::BufferInitDescriptor {
            label: Some("Index Buffer"),
            contents: bytemuck::cast_slice(&frame.indices),
            usage: wgpu::BufferUsages::INDEX,
        },
    );

    // --- Render Pass ---
    {
        let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Render Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::WHITE),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });

        rpass.set_pipeline(&pipeline);
        rpass.set_bind_group(0, &bind_group, &[]);
        rpass.set_vertex_buffer(0, vertex_buffer.slice(..));
        rpass.set_index_buffer(index_buffer.slice(..), wgpu::IndexFormat::Uint32);
        rpass.draw_indexed(0..frame.indices.len() as u32, 0, 0..1);
    }

    // --- Copy texture → buffer ---
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::ImageCopyBuffer {
            buffer: &output_buffer,
            layout: wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );

    queue.submit(Some(encoder.finish()));

    // --- Map buffer ---
    let buffer_slice = output_buffer.slice(..);

    let (tx, rx) = tokio::sync::oneshot::channel();
    buffer_slice.map_async(wgpu::MapMode::Read, move |v| {
        tx.send(v).unwrap();
    });

    device.poll(wgpu::Maintain::Wait);

    rx.await??;

    let data = buffer_slice.get_mapped_range();

    // Remove row padding
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);

    for chunk in data.chunks(padded_bytes_per_row as usize) {
        pixels.extend_from_slice(&chunk[..unpadded_bytes_per_row as usize]);
    }

    drop(data);
    output_buffer.unmap();

    // --- Encode PNG ---
    let img: ImageBuffer<Rgba<u8>, _> =
        ImageBuffer::from_raw(width, height, pixels)
            .ok_or_else(|| anyhow::anyhow!("Failed to create image buffer"))?;

    let mut png_bytes = Vec::new();
    img.write_to(
        &mut std::io::Cursor::new(&mut png_bytes),
        image::ImageFormat::Png,
    )?;

    Ok(png_bytes)
}