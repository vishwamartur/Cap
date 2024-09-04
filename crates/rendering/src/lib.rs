use anyhow::Result;
use bytemuck::{Pod, Zeroable};
use ffmpeg_next::{frame, rescale, Rational, Rescale};
use futures_intrusive::channel::shared::oneshot_channel;
use serde::{Deserialize, Serialize};
use specta::Type;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::thread;
use wgpu::util::DeviceExt;
use wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

use cap_project::{BackgroundSource, CameraXPosition, CameraYPosition, ProjectConfiguration};

use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct RenderOptions {
    pub camera_size: Option<(u32, u32)>,
    pub screen_size: (u32, u32),
    pub output_size: (u32, u32),
}

#[derive(Debug, Clone, Serialize, Deserialize, Type)]
pub struct WebcamStyle {
    pub border_radius: f32,
    pub shadow_color: [f32; 4],
    pub shadow_blur: f32,
    pub shadow_offset: (f32, f32),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Type, Pod, Zeroable)]
#[repr(C)]
pub struct CompositeParams {
    // static inputs
    background_start: [f32; 4],
    background_end: [f32; 4],

    shadow_color: [f32; 4],
    shadow_offset: [f32; 2],

    webcam_position: [f32; 2],
    // webcam_size: [f32; 2],
    screen_padding: f32,
    screen_rounding: f32,
    camera_rounding: f32,

    _padding1: f32,

    shadow_blur: f32,

    background_angle: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Type, Copy)]
pub enum Background {
    Color([f32; 4]),
    Gradient {
        start: [f32; 4],
        end: [f32; 4],
        angle: f32,
    },
}

impl From<BackgroundSource> for Background {
    fn from(value: BackgroundSource) -> Self {
        match value {
            BackgroundSource::Color { value } => Background::Color([
                srgb_to_linear(value[0]),
                srgb_to_linear(value[1]),
                srgb_to_linear(value[2]),
                1.0,
            ]),
            BackgroundSource::Gradient { from, to } => Background::Gradient {
                start: [
                    srgb_to_linear(from[0]),
                    srgb_to_linear(from[1]),
                    srgb_to_linear(from[2]),
                    1.0,
                ],
                end: [
                    srgb_to_linear(to[0]),
                    srgb_to_linear(to[1]),
                    srgb_to_linear(to[2]),
                    1.0,
                ],
                angle: 0.0,
            },
            _ => unimplemented!(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct UV<T> {
    pub u: T,
    pub v: T,
}

impl<T> UV<T> {
    pub fn new(x: T, y: T) -> Self {
        Self { u: x, v: y }
    }
}

impl<T> From<UV<T>> for (T, T) {
    fn from(xy: UV<T>) -> Self {
        (xy.u, xy.v)
    }
}

impl<T> From<UV<T>> for [T; 2] {
    fn from(xy: UV<T>) -> Self {
        [xy.u, xy.v]
    }
}

pub async fn render_video_to_channel(
    options: RenderOptions,
    project: ProjectConfiguration,
    sender: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    screen_recording_decoder: VideoDecoderActor,
    camera_recording_decoder: Option<VideoDecoderActor>,
) -> Result<(), String> {
    let constants = RenderVideoConstants::new(options).await?;


    println!("Setting up FFmpeg input for screen recording...");

    ffmpeg_next::init().unwrap();

    let (screen_tx, mut screen_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    tokio::spawn(async move {
        let now = Instant::now();

        let mut i = 0;
        loop {
            match screen_recording_decoder.get_frame(i).await {
                Some(frame) => {
                    if screen_tx.send(frame).is_err() {
                        println!("Error sending screen frame to renderer");
                        break;
                    }
                }
                None => {
                    println!("Reached end of screen stream");
                    break;
                }
            }
            i += 1;
        }

        println!("done decoding screen in {:.2?}", now.elapsed())
    });

    let mut camera_rx = camera_recording_decoder.map(|camera_recording_decoder| {
        println!("Setting up FFmpeg input for webcam recording...");
        let (camera_tx, camera_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

        tokio::spawn(async move {
            let now = Instant::now();

            let mut i = 0;
            loop {
                match camera_recording_decoder.get_frame(i).await {
                    Some(frame) => {
                        if camera_tx.send(frame).is_err() {
                            println!("Error sending screen frame to renderer");
                            break;
                        }
                    }
                    None => {
                        println!("Reached end of screen stream");
                        break;
                    }
                }
                i += 1;
            }

            println!("done decoding camera in {:.2?}", now.elapsed())
        });

        camera_rx
    });

    let start_time = Instant::now();

    let render_handle: tokio::task::JoinHandle<Result<u32, String>> = tokio::spawn(async move {
        let mut frame_count = 0;

        let uniforms = ProjectUniforms::new(&constants, &project);
        let background = Background::from(project.background.source);

        loop {
            let Some(screen_frame) = screen_rx.recv().await else {
                break;
            };
            let camera_frame = match &mut camera_rx {
                Some(rx) => rx.recv().await,
                None => None,
            };

            let frame = match produce_frame(
                &constants,
                &screen_frame,
                camera_frame.as_ref(),
                background,
                &uniforms,
            )
            .await
            {
                Ok(frame) => frame,
                Err(e) => {
                    eprintln!("{e}");
                    break;
                }
            };

            if sender.send(frame).is_err() {
                eprintln!("Failed to send processed frame to FFmpeg");
                break;
            }

            frame_count += 1;
            if frame_count % 60 == 0 {
                let elapsed = start_time.elapsed();
                println!(
                    "Processed {} frames in {:?} seconds",
                    frame_count,
                    elapsed.as_secs_f32()
                );
            }
        }

        println!("Render loop exited");

        Ok(frame_count)
    });

    let total_frames = render_handle.await.map_err(|e| e.to_string())??;

    let total_time = start_time.elapsed();
    println!(
        "Render complete. Processed {} frames in {:?} seconds",
        total_frames,
        total_time.as_secs_f32()
    );

    Ok(())
}

pub struct RenderVideoConstants {
    pub _instance: wgpu::Instance,
    pub _adapter: wgpu::Adapter,
    pub queue: wgpu::Queue,
    pub device: wgpu::Device,
    pub options: RenderOptions,
    pub composite_video_frame_pipeline: CompositeVideoFramePipeline,
    pub gradient_or_color_pipeline: GradientOrColorPipeline,
    pub output_texture: wgpu::Texture,
    pub output_texture_view: wgpu::TextureView,
    pub output_texture_size: wgpu::Extent3d,
    pub output_buffer: wgpu::Buffer,
    pub padded_bytes_per_row: u32,
    pub unpadded_bytes_per_row: u32,
}

impl RenderVideoConstants {
    pub async fn new(options: RenderOptions) -> Result<Self, String> {
        println!("Initializing wgpu...");
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .unwrap();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default(), None)
            .await
            .map_err(|e| e.to_string())?;

        let output_texture_size = wgpu::Extent3d {
            width: options.output_size.0,
            height: options.output_size.1,
            depth_or_array_layers: 1,
        };

        let output_texture = device.create_texture(
            &(wgpu::TextureDescriptor {
                size: output_texture_size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                label: Some("output_texture"),
                view_formats: &[],
            }),
        );

        // Calculate the aligned bytes per row
        let align = COPY_BYTES_PER_ROW_ALIGNMENT;
        let unpadded_bytes_per_row = options.output_size.0 * 4;
        let padding = (align - (unpadded_bytes_per_row % align)) % align;
        let padded_bytes_per_row = unpadded_bytes_per_row + padding;

        // Ensure the padded_bytes_per_row is a multiple of 4 (32 bits)
        let padded_bytes_per_row = (padded_bytes_per_row + 3) & !3;

        let output_buffer_size = (padded_bytes_per_row * options.output_size.1) as u64;

        let output_buffer = device.create_buffer(
            &(wgpu::BufferDescriptor {
                size: output_buffer_size,
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                label: Some("Output Buffer"),
                mapped_at_creation: false,
            }),
        );

        Ok(Self {
            composite_video_frame_pipeline: CompositeVideoFramePipeline::new(&device),
            gradient_or_color_pipeline: GradientOrColorPipeline::new(&device),
            _instance: instance,
            _adapter: adapter,
            queue,
            device,
            options,
            output_texture_size,
            output_texture_view: output_texture
                .create_view(&wgpu::TextureViewDescriptor::default()),
            output_texture,
            output_buffer,
            unpadded_bytes_per_row,
            padded_bytes_per_row,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ProjectUniforms {
    display: CompositeVideoFrameUniforms,
    camera: Option<CompositeVideoFrameUniforms>,
}

const CAMERA_PADDING: f32 = 50.0;
const CAMERA_UV_WIDTH: f32 = 0.3;

const SCREEN_MAX_PADDING: f32 = 0.4;

impl ProjectUniforms {
    pub fn new(constants: &RenderVideoConstants, project: &ProjectConfiguration) -> Self {
        let options = &constants.options;
        let output_size = [options.output_size.0 as f32, options.output_size.1 as f32];

        let display = {
            let frame_size = [options.screen_size.0 as f32, options.screen_size.1 as f32];
            let frame_aspect = frame_size[0] / frame_size[1];
            let min_axis = frame_size[0].min(frame_size[1]);
            let y_padding =
                project.background.padding as f32 / 100.0 * SCREEN_MAX_PADDING * output_size[1];

            let target_height = (output_size[1] - y_padding) - y_padding;
            let target_width = target_height * frame_aspect;
            let target_left_bounds = (output_size[0] - target_width) / 2.0;
            let target_bounds = [
                target_left_bounds,
                y_padding,
                output_size[0] - target_left_bounds,
                output_size[1] - y_padding,
            ];
            let target_size = [target_bounds[2] - target_bounds[0], target_height];
            let min_target_axis = target_size[0].min(target_size[1]);

            CompositeVideoFrameUniforms {
                output_size,
                frame_size,
                crop_bounds: [0.0, 0.0, frame_size[0], frame_size[1]],
                target_bounds,
                target_size,
                rounding_px: project.background.rounding as f32 / 100.0 * 0.5 * min_target_axis,
                ..Default::default()
            }
        };

        let camera = options
            .camera_size
            .filter(|_| !project.camera.hide)
            .map(|camera_size| {
                let frame_size = [camera_size.0 as f32, camera_size.1 as f32];
                let min_axis = frame_size[0].min(frame_size[1]);

                let size = [
                    min_axis * CAMERA_UV_WIDTH + CAMERA_PADDING,
                    min_axis * CAMERA_UV_WIDTH + CAMERA_PADDING,
                ];

                let position = {
                    let x = match &project.camera.position.x {
                        CameraXPosition::Left => CAMERA_PADDING,
                        CameraXPosition::Center => output_size[0] / 2.0 - (size[0]) / 2.0,
                        CameraXPosition::Right => output_size[0] - CAMERA_PADDING - size[0],
                    };
                    let y = match &project.camera.position.y {
                        CameraYPosition::Top => CAMERA_PADDING,
                        CameraYPosition::Bottom => output_size[1] - size[1] - CAMERA_PADDING,
                    };

                    [x, y]
                };

                let target_bounds = [
                    position[0],
                    position[1],
                    position[0] + size[0],
                    position[1] + size[1],
                ];

                CompositeVideoFrameUniforms {
                    output_size,
                    frame_size,
                    crop_bounds: [
                        (frame_size[0] - frame_size[1]) / 2.0,
                        0.0,
                        frame_size[0] - (frame_size[0] - frame_size[1]) / 2.0,
                        frame_size[1],
                    ],
                    target_bounds,
                    target_size: [
                        target_bounds[2] - target_bounds[0],
                        target_bounds[3] - target_bounds[1],
                    ],
                    rounding_px: project.camera.rounding as f32 / 100.0 * 0.5 * size[0],
                    mirror_x: if project.camera.mirror { 1.0 } else { 0.0 },
                    ..Default::default()
                }
            });

        Self { display, camera }
    }
}

pub async fn produce_frame(
    RenderVideoConstants {
        device,
        options,
        composite_video_frame_pipeline,
        gradient_or_color_pipeline,
        queue,
        output_texture_size,
        output_buffer,
        padded_bytes_per_row,
        unpadded_bytes_per_row,
        ..
    }: &RenderVideoConstants,
    screen_frame: &Vec<u8>,
    camera_frame: Option<&Vec<u8>>,
    background: Background,
    uniforms: &ProjectUniforms,
) -> Result<Vec<u8>, String> {
    let mut encoder = device.create_command_encoder(
        &(wgpu::CommandEncoderDescriptor {
            label: Some("Render Encoder"),
        }),
    );

    let output_texture_desc = wgpu::TextureDescriptor {
        size: wgpu::Extent3d {
            width: options.output_size.0,
            height: options.output_size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::COPY_SRC,
        label: Some("Intermediate Texture"),
        view_formats: &[],
    };

    let textures = (
        device.create_texture(&output_texture_desc),
        device.create_texture(&output_texture_desc),
    );

    let textures = (&textures.0, &textures.1);

    let texture_views = (
        textures
            .0
            .create_view(&wgpu::TextureViewDescriptor::default()),
        textures
            .1
            .create_view(&wgpu::TextureViewDescriptor::default()),
    );

    let texture_views = (&texture_views.0, &texture_views.1);

    let mut output_is_left = true;

    {
        do_render_pass(
            &mut encoder,
            get_either(texture_views, output_is_left),
            &gradient_or_color_pipeline.render_pipeline,
            gradient_or_color_pipeline.bind_group(
                device,
                &GradientOrColorUniforms::from(background).to_buffer(device),
            ),
        );

        output_is_left = !output_is_left;
    }

    {
        let frame_size = options.screen_size;

        let texture = device.create_texture(
            &(wgpu::TextureDescriptor {
                size: wgpu::Extent3d {
                    width: options.screen_size.0,
                    height: options.screen_size.1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_DST,
                label: Some("Screen Frame texture"),
                view_formats: &[],
            }),
        );

        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            screen_frame,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(options.screen_size.0 * 4),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: frame_size.0,
                height: frame_size.1,
                depth_or_array_layers: 1,
            },
        );

        // let padding = 30.0;
        // let frame_size = (frame_size.0 as f32, frame_size.1 as f32);
        // let output_size = (options.output_size.0 as f32, options.output_size.1 as f32);
        // let x_scale = output_size.0 / frame_size.0;
        // let target_bounds = [
        //     padding,
        //     padding,
        //     output_size.0 - padding,
        //     frame_size.1 * x_scale - padding * x_scale,
        // ];
        // let target_size = [
        //     target_bounds[2] - target_bounds[0],
        //     target_bounds[3] - target_bounds[1],
        // ];
        // let rounding_pc = 0.1;
        // let rounding_px = f32::min(target_size[0], target_size[1]) * 0.5 * rounding_pc;

        do_render_pass(
            &mut encoder,
            get_either(texture_views, output_is_left),
            &composite_video_frame_pipeline.render_pipeline,
            composite_video_frame_pipeline.bind_group(
                device,
                &uniforms.display.to_buffer(device),
                // &dbg!(CompositeVideoFrameUniforms {
                //     output_size: [output_size.0, output_size.1],
                //     frame_size: [frame_size.0, frame_size.1],
                //     crop_bounds: [0.0, 0.0, frame_size.0, frame_size.1],
                //     target_bounds,
                //     target_size,
                //     rounding_px,
                //     ..Default::default()
                // })
                // .to_buffer(device),
                &texture_view,
                get_either(texture_views, !output_is_left),
            ),
        );

        output_is_left = !output_is_left;
    }

    if let (Some(camera_size), Some(camera_frame), Some(uniforms)) =
        (options.camera_size, camera_frame, &uniforms.camera)
    {
        let texture = device.create_texture(
            &(wgpu::TextureDescriptor {
                size: wgpu::Extent3d {
                    width: camera_size.0,
                    height: camera_size.1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                usage: wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::COPY_DST,
                label: Some("Camera texture"),
                view_formats: &[],
            }),
        );

        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        println!("camera frame size: {}", camera_frame.len());

        queue.write_texture(
            wgpu::ImageCopyTexture {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            camera_frame,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(camera_size.0 * 4),
                rows_per_image: None,
            },
            wgpu::Extent3d {
                width: camera_size.0,
                height: camera_size.1,
                depth_or_array_layers: 1,
            },
        );

        // let padding = 30.0;
        // let frame_size = (camera_size.0 as f32, camera_size.1 as f32);
        // let output_size = (options.output_size.0 as f32, options.output_size.1 as f32);
        // let x_scale = output_size.0 / frame_size.0;
        // let target_bounds = [padding, padding, 300.0, 300.0];
        // let target_size = [
        //     target_bounds[2] - target_bounds[0],
        //     target_bounds[3] - target_bounds[1],
        // ];
        // let rounding_pc = 0.1;
        // let rounding_px = f32::min(target_size[0], target_size[1]) * 0.5 * rounding_pc;

        do_render_pass(
            &mut encoder,
            get_either(texture_views, output_is_left),
            &composite_video_frame_pipeline.render_pipeline,
            composite_video_frame_pipeline.bind_group(
                device,
                &uniforms.to_buffer(device),
                // &dbg!(CompositeVideoFrameUniforms {
                //     output_size: [output_size.0, output_size.1],
                //     frame_size: [frame_size.0, frame_size.1],
                //     crop_bounds: [
                //         (frame_size.0 - frame_size.1) / 2.0,
                //         0.0,
                //         frame_size.0 - (frame_size.0 - frame_size.1) / 2.0,
                //         frame_size.1
                //     ],
                //     target_bounds,
                //     target_size,
                //     rounding_px,
                //     ..Default::default()
                // })
                // .to_buffer(device),
                &texture_view,
                get_either(texture_views, !output_is_left),
            ),
        );

        output_is_left = !output_is_left;
    }

    queue.submit(std::iter::once(encoder.finish()));

    {
        let mut encoder = device.create_command_encoder(
            &(wgpu::CommandEncoderDescriptor {
                label: Some("Copy Encoder"),
            }),
        );

        encoder.copy_texture_to_buffer(
            wgpu::ImageCopyTexture {
                texture: get_either(textures, !output_is_left),
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::ImageCopyBuffer {
                buffer: output_buffer,
                layout: wgpu::ImageDataLayout {
                    offset: 0,
                    bytes_per_row: Some(*padded_bytes_per_row),
                    rows_per_image: Some(options.output_size.1),
                },
            },
            *output_texture_size,
        );

        queue.submit(std::iter::once(encoder.finish()));
    }

    let buffer_slice = output_buffer.slice(..);
    let (tx, rx) = oneshot_channel();
    buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
        tx.send(result).ok();
    });
    device.poll(wgpu::Maintain::Wait);

    let Some(frame_result) = rx.receive().await else {
        return Err("2: Channel closed unexpectedly".to_string());
    };

    if let Err(e) = frame_result {
        return Err(format!("Failed to map buffer: {:?}", e));
    }

    let data = buffer_slice.get_mapped_range();
    let padded_data: Vec<u8> = data.to_vec(); // Ensure the type is Vec<u8>
    let mut image_data =
        Vec::with_capacity((options.output_size.0 * options.output_size.1 * 4) as usize);
    for chunk in padded_data.chunks(*padded_bytes_per_row as usize) {
        image_data.extend_from_slice(&chunk[..*unpadded_bytes_per_row as usize]);
    }

    // Unmap the buffer
    drop(data);
    output_buffer.unmap();

    Ok(image_data)
}

struct CompositeVideoFramePipeline {
    pub bind_group_layout: wgpu::BindGroupLayout,
    pub render_pipeline: wgpu::RenderPipeline,
}

#[derive(Debug, Clone, Copy, Pod, Zeroable, Default)]
#[repr(C)]
struct CompositeVideoFrameUniforms {
    pub crop_bounds: [f32; 4],
    pub target_bounds: [f32; 4],
    pub output_size: [f32; 2],
    pub frame_size: [f32; 2],
    pub velocity_uv: [f32; 2],
    pub target_size: [f32; 2],
    pub rounding_px: f32,
    pub mirror_x: f32,
    _padding: [f32; 3],
}

impl CompositeVideoFrameUniforms {
    fn to_buffer(self, device: &wgpu::Device) -> wgpu::Buffer {
        device.create_buffer_init(
            &(wgpu::util::BufferInitDescriptor {
                label: Some("CompositeVideoFrameUniforms Buffer"),
                contents: bytemuck::cast_slice(&[self]),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            }),
        )
    }
}

impl CompositeVideoFramePipeline {
    pub fn new(device: &wgpu::Device) -> Self {
        let bind_group_layout = Self::bind_group_layout(device);
        let render_pipeline =
            create_shader_render_pipeline(device, &bind_group_layout, Self::shader());

        Self {
            bind_group_layout,
            render_pipeline,
        }
    }

    fn shader() -> &'static str {
        include_str!("shaders/composite-video-frame.wgsl")
    }

    fn bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("composite-video-frame.wgsl Bind Group Layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        })
    }

    pub fn bind_group(
        &self,
        device: &wgpu::Device,
        uniforms: &wgpu::Buffer,
        frame: &wgpu::TextureView,
        intermediate: &wgpu::TextureView,
    ) -> wgpu::BindGroup {
        let sampler = device.create_sampler(
            &(wgpu::SamplerDescriptor {
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                address_mode_w: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::FilterMode::Nearest,
                ..Default::default()
            }),
        );

        let bind_group = device.create_bind_group(
            &(wgpu::BindGroupDescriptor {
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: uniforms.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(frame),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::TextureView(intermediate),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: wgpu::BindingResource::Sampler(&sampler),
                    },
                ],
                label: Some("bind_group"),
            }),
        );

        bind_group
    }
}

struct GradientOrColorPipeline {
    pub bind_group_layout: wgpu::BindGroupLayout,
    pub render_pipeline: wgpu::RenderPipeline,
}

#[derive(Debug, Clone, Copy, Pod, Zeroable, Default)]
#[repr(C)]
struct GradientOrColorUniforms {
    pub start: [f32; 4],
    pub end: [f32; 4],
    pub angle: f32,
    _padding: [f32; 3],
}

impl GradientOrColorUniforms {
    fn to_buffer(self, device: &wgpu::Device) -> wgpu::Buffer {
        device.create_buffer_init(
            &(wgpu::util::BufferInitDescriptor {
                label: Some("GradientOrColorUniforms Buffer"),
                contents: bytemuck::cast_slice(&[self]),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            }),
        )
    }
}

impl From<Background> for GradientOrColorUniforms {
    fn from(value: Background) -> Self {
        match value {
            Background::Color(color) => Self {
                start: color,
                end: color,
                angle: 0.0,
                _padding: [0.0; 3],
            },
            Background::Gradient { start, end, angle } => Self {
                start,
                end,
                angle,
                _padding: [0.0; 3],
            },
        }
    }
}

impl GradientOrColorPipeline {
    pub fn new(device: &wgpu::Device) -> Self {
        let bind_group_layout = Self::bind_group_layout(device);
        let render_pipeline =
            create_shader_render_pipeline(device, &bind_group_layout, Self::shader());

        Self {
            bind_group_layout,
            render_pipeline,
        }
    }

    fn shader() -> &'static str {
        include_str!("shaders/gradient-or-color.wgsl")
    }

    fn bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
        device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("composite-video-frame.wgsl Bind Group Layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        })
    }

    pub fn bind_group(&self, device: &wgpu::Device, uniforms: &wgpu::Buffer) -> wgpu::BindGroup {
        let bind_group = device.create_bind_group(
            &(wgpu::BindGroupDescriptor {
                layout: &self.bind_group_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniforms.as_entire_binding(),
                }],
                label: Some("bind_group"),
            }),
        );

        bind_group
    }
}

fn do_render_pass(
    encoder: &mut wgpu::CommandEncoder,
    output_view: &wgpu::TextureView,
    render_pipeline: &wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
) {
    let mut render_pass = encoder.begin_render_pass(
        &(wgpu::RenderPassDescriptor {
            label: Some("Render Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: output_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        }),
    );

    render_pass.set_pipeline(render_pipeline);
    render_pass.set_bind_group(0, &bind_group, &[]);
    render_pass.draw(0..3, 0..1);
}

fn create_shader_render_pipeline(
    device: &wgpu::Device,
    bind_group_layout: &wgpu::BindGroupLayout,
    wgsl_shader: &str,
) -> wgpu::RenderPipeline {
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("Shader"),
        source: wgpu::ShaderSource::Wgsl(wgsl_shader.into()),
    });

    let compilation_options = wgpu::PipelineCompilationOptions {
        constants: &HashMap::new(),
        zero_initialize_workgroup_memory: false,
        vertex_pulling_transform: false,
    };

    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("Render Pipeline"),
        layout: Some(&device.create_pipeline_layout(
            &(wgpu::PipelineLayoutDescriptor {
                label: Some("Render Pipeline Layout"),
                bind_group_layouts: &[bind_group_layout],
                push_constant_ranges: &[],
            }),
        )),
        vertex: wgpu::VertexState {
            module: &shader,
            entry_point: "vs_main",
            buffers: &[],
            compilation_options: compilation_options.clone(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &shader,
            entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options,
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            strip_index_format: None,
            front_face: wgpu::FrontFace::Ccw,
            cull_mode: Some(wgpu::Face::Back),
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState {
            count: 1,
            mask: !0,
            alpha_to_coverage_enabled: false,
        },
        multiview: None,
        cache: None,
    })
}

fn srgb_to_linear(c: u16) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

struct VideoDecoder {
    input: ffmpeg_next::format::context::Input,
    decoder: ffmpeg_next::codec::decoder::Video,
    scaler: ffmpeg_next::software::scaling::context::Context,
    input_stream_index: usize,
    temp_frame: ffmpeg_next::frame::Video,
    time_base: ffmpeg_next::Rational,
    frame_rate: ffmpeg_next::Rational,
    last_frame: Option<(u32, Vec<u8>)>,
    // cached_frames: HashMap<u32, Vec<u8>>,
}

impl VideoDecoder {
    fn new(path: &PathBuf) -> Self {
        println!("creating decoder for {}", path.display());
        let ictx = ffmpeg_next::format::input(path).unwrap();

        let input_stream = ictx
            .streams()
            .best(ffmpeg_next::media::Type::Video)
            .ok_or("Could not find a video stream")
            .unwrap();
        let input_stream_index = input_stream.index();
        let time_base = input_stream.time_base();
        let frame_rate = input_stream.rate();

        // Create a decoder for the video stream
        let decoder =
            ffmpeg_next::codec::context::Context::from_parameters(input_stream.parameters())
                .unwrap()
                .decoder()
                .video()
                .unwrap();

        use ffmpeg_next::format::Pixel;
        use ffmpeg_next::software::scaling::{context::Context, flag::Flags};

        let scaler = Context::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            Pixel::RGBA,
            decoder.width(),
            decoder.height(),
            Flags::BILINEAR,
        )
        .unwrap();

        Self {
            input: ictx,
            decoder,
            scaler,
            input_stream_index,
            temp_frame: ffmpeg_next::frame::Video::empty(),
            time_base,
            frame_rate,
            last_frame: None,
            // cached_frames: Default::default(),
        }
    }

    // TODO: optimisations
    // - cache frames
    // - cache I frame positions to know whether seeking is actually necessary when going backwards
    fn get_frame(&mut self, frame_number: u32) -> Option<Vec<u8>> {
        if let Some((last_frame_number, frame)) = &self.last_frame {
            if *last_frame_number == frame_number {
                return Some(frame.clone());
            }
        }

        if self.last_frame.is_some()
            && (frame_number <= 0 || frame_number <= self.last_frame.as_ref().unwrap().0)
        {
            let timestamp_us =
                ((frame_number as f32 / self.frame_rate.numerator() as f32) * 1_000_000.0) as i64;
            let position = timestamp_us.rescale((1, 1_000_000), rescale::TIME_BASE);

            println!("seeeking to {position} for frame {frame_number}");

            self.decoder.flush();
            self.input.seek(position, ..position).unwrap();
        }

        for (stream, packet) in self.input.packets() {
            if stream.index() == self.input_stream_index {
                let current_frame =
                    ts_to_frame(packet.pts().unwrap(), self.time_base, self.frame_rate);

                self.decoder.send_packet(&packet).unwrap();

                while self.decoder.receive_frame(&mut self.temp_frame).is_ok() {
                    // Convert the frame to RGB
                    let mut rgb_frame = frame::Video::empty();
                    self.scaler.run(&self.temp_frame, &mut rgb_frame).unwrap();

                    let frame = rgb_frame.data(0).to_vec();

                    self.last_frame = Some((current_frame, frame.clone()));

                    if current_frame == frame_number {
                        return Some(frame);
                    }
                }
            }
        }

        None
    }
}

enum VideoDecoderActorMessage {
    GetFrame(u32, tokio::sync::oneshot::Sender<Option<Vec<u8>>>),
}

#[derive(Clone)]
pub struct VideoDecoderActor {
    tx: Sender<VideoDecoderActorMessage>,
}

impl VideoDecoderActor {
    pub fn new(path: PathBuf) -> Self {
        let (tx, rx) = channel();

        thread::spawn(move || {
            let mut decoder = VideoDecoder::new(&path);

            loop {
                match rx.recv() {
                    Ok(VideoDecoderActorMessage::GetFrame(frame_number, sender)) => {
                        let frame = decoder.get_frame(frame_number);
                        sender.send(frame).ok();
                    }
                    Err(_) => break,
                }
            }
        });

        Self { tx }
    }

    pub async fn get_frame(&self, frame_number: u32) -> Option<Vec<u8>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(VideoDecoderActorMessage::GetFrame(frame_number, tx))
            .unwrap();
        rx.await.unwrap()
    }
}

fn ts_to_frame(ts: i64, time_base: Rational, frame_rate: Rational) -> u32 {
    // dbg!((ts, time_base, frame_rate));
    ((ts * time_base.numerator() as i64 * frame_rate.numerator() as i64)
        / (time_base.denominator() as i64 * frame_rate.denominator() as i64)) as u32
}

fn get_either<T>((a, b): (T, T), left: bool) -> T {
    if left {
        a
    } else {
        b
    }
}
