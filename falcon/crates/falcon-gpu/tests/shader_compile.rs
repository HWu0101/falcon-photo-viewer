//! Headless GPU validation that the two fast-tier shaders (`YUV_WGSL` via `YuvConvert`, and `CM_SHADER`)
//! actually COMPILE on a real device — naga/wgpu validate WGSL at shader-module + pipeline creation, which
//! the CPU-mirror parity tests do NOT exercise. This catches a malformed shader (bad `textureLoad`, an
//! undeclared binding, a signature mismatch) at test time instead of at app boot. Gracefully SKIPS when no
//! GPU adapter is available (headless CI) — the app's boot is the production backstop either way.

#[path = "../../test-support/hw_child.rs"]
mod hw_child;

fn headless() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()?;
    Some((device, queue))
}

#[test]
fn yuv_pipeline_compiles() {
    if !hw_child::run_here(module_path!(), "yuv_pipeline_compiles") { return; }
    let Some((device, _queue)) = headless() else {
        eprintln!("shader_compile: no GPU adapter — skipping (boot is the backstop)");
        return;
    };
    // YuvConvert::new builds the shader module + the full render pipeline from YUV_WGSL (incl. the
    // v0.8.47 lut_tex binding + LUT encode). A WGSL error surfaces as an Err/panic here.
    falcon_gpu::YuvConvert::new(&device).expect("YUV_WGSL must compile + build its pipeline");
}

/// v0.8.145 (E2): the third shader in the crate — the NV12→RGB8 kernel's GPU twin. Compute, not
/// render, so nothing above validates it; `Nv12Kernel::new` builds the module AND the compute
/// pipeline, which is where naga runs full validation of the integer arithmetic, the Uint texture
/// bindings and the storage-buffer write.
#[test]
fn nv12_kernel_pipeline_compiles() {
    if !hw_child::run_here(module_path!(), "nv12_kernel_pipeline_compiles") { return; }
    let Some((device, _queue)) = headless() else {
        eprintln!("shader_compile: no GPU adapter — skipping (boot is the backstop)");
        return;
    };
    falcon_gpu::Nv12Kernel::new(&device).expect("NV12_RGB_WGSL must compile + build its pipeline");
}

#[test]
fn cm_shader_compiles() {
    if !hw_child::run_here(module_path!(), "cm_shader_compiles") { return; }
    let Some((device, _queue)) = headless() else {
        eprintln!("shader_compile: no GPU adapter — skipping");
        return;
    };
    // Build the CM pipeline from falcon_gpu::CM_SHADER with the SAME bind group layout support.rs uses
    // (src texture, non-filtering sampler, uniform, + the v0.8.47 lut_tex texture at binding 3).
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("cm-compile-test"),
        source: wgpu::ShaderSource::Wgsl(falcon_gpu::CM_SHADER.into()),
    });
    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: None,
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 3,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
        ],
    });
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: None,
        bind_group_layouts: &[Some(&bgl)],
        immediate_size: 0,
    });
    // Pipeline creation runs full validation (entry points, bindings, types).
    let _pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("cm-compile-test"),
        layout: Some(&pl),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs"),
            buffers: &[],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        },
        primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, ..Default::default() },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs"),
            targets: &[Some(wgpu::ColorTargetState {
                format: wgpu::TextureFormat::Rgba8Unorm,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: wgpu::PipelineCompilationOptions::default(),
        }),
        multiview_mask: None,
        cache: None,
    });
}
