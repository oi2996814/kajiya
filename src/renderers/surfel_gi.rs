use std::{mem::size_of, sync::Arc};

use slingshot::{
    ash::vk,
    backend::{
        buffer::{Buffer, BufferDesc},
        device,
        image::*,
        ray_tracing::RayTracingAcceleration,
        shader::*,
        RenderBackend,
    },
    rg::{self, BindRgRef, Resource},
    vk_sync::AccessType,
};

use crate::temporal::*;

pub struct SurfelGiRenderer {
    surfel_meta_buf: Temporal<Buffer>,
    surfel_hash_key_buf: Temporal<Buffer>,
    surfel_hash_value_buf: Temporal<Buffer>,

    cell_index_offset_buf: Temporal<Buffer>,
    surfel_index_buf: Temporal<Buffer>,

    surfel_spatial_buf: Temporal<Buffer>,
    surfel_irradiance_buf: Temporal<Buffer>,
}

const MAX_SURFEL_CELLS: usize = 1024 * 1024;
const MAX_SURFELS: usize = MAX_SURFEL_CELLS;
const MAX_SURFELS_PER_CELL: usize = 8;

macro_rules! impl_renderer_temporal_logic {
    ($($res_name:ident,)*) => {
        pub fn begin(&mut self, rg: &mut rg::RenderGraph) -> SurfelGiRenderInstance {
            SurfelGiRenderInstance {
                $(
                    $res_name: rg.import_temporal(&mut self.$res_name),
                )*
            }
        }
        pub fn end(&mut self, rg: &mut rg::RenderGraph, inst: SurfelGiRenderInstance) {
            $(
                rg.export_temporal(inst.$res_name, &mut self.$res_name);
            )*
        }

        pub fn retire(&mut self, rg: &rg::RetiredRenderGraph) {
            $(
                rg.retire_temporal(&mut self.$res_name);
            )*
        }
    };
}

fn new_temporal_storage_buffer(device: &device::Device, size_bytes: usize) -> Temporal<Buffer> {
    Temporal::new(Arc::new(
        device
            .create_buffer(
                BufferDesc::new(size_bytes, vk::BufferUsageFlags::STORAGE_BUFFER),
                None,
            )
            .unwrap(),
    ))
}

impl SurfelGiRenderer {
    pub fn new(device: &device::Device) -> Self {
        Self {
            // 0: hash grid cell count
            // 1: surfel count
            surfel_meta_buf: new_temporal_storage_buffer(device, size_of::<u32>() * 8),
            surfel_hash_key_buf: new_temporal_storage_buffer(
                device,
                size_of::<u32>() * MAX_SURFEL_CELLS,
            ),
            surfel_hash_value_buf: new_temporal_storage_buffer(
                device,
                size_of::<u32>() * MAX_SURFEL_CELLS,
            ),
            cell_index_offset_buf: new_temporal_storage_buffer(
                device,
                size_of::<u32>() * MAX_SURFEL_CELLS,
            ),
            surfel_index_buf: new_temporal_storage_buffer(
                device,
                size_of::<u32>() * MAX_SURFEL_CELLS * MAX_SURFELS_PER_CELL,
            ),
            surfel_spatial_buf: new_temporal_storage_buffer(device, 16 * MAX_SURFELS),
            surfel_irradiance_buf: new_temporal_storage_buffer(device, 32 * MAX_SURFELS),
        }
    }

    impl_renderer_temporal_logic! {
        surfel_meta_buf,
        surfel_hash_key_buf,
        surfel_hash_value_buf,
        cell_index_offset_buf,
        surfel_index_buf,
        surfel_spatial_buf,
        surfel_irradiance_buf,
    }
}

pub struct SurfelGiRenderInstance {
    pub surfel_meta_buf: rg::Handle<Buffer>,
    pub surfel_hash_key_buf: rg::Handle<Buffer>,
    pub surfel_hash_value_buf: rg::Handle<Buffer>,

    pub cell_index_offset_buf: rg::Handle<Buffer>,
    pub surfel_index_buf: rg::Handle<Buffer>,

    pub surfel_spatial_buf: rg::Handle<Buffer>,
    pub surfel_irradiance_buf: rg::Handle<Buffer>,
}

impl SurfelGiRenderInstance {
    pub fn allocate_surfels(
        &mut self,
        rg: &mut rg::RenderGraph,
        gbuffer: &rg::Handle<Image>,
        depth: &rg::Handle<Image>,
    ) -> rg::Handle<Image> {
        let mut pass = rg.add_pass();
        let mut debug_out = pass.create(&gbuffer.desc().format(vk::Format::R32G32B32A32_SFLOAT));

        let mut tile_surfel_alloc_tex = pass.create(
            &gbuffer
                .desc()
                .div_up_extent([8, 8, 1])
                .format(vk::Format::R32G32_UINT),
        );

        SimpleComputePass::new(pass, "/assets/shaders/surfel_gi/find_missing_surfels.hlsl")
            .read(&gbuffer)
            .read_aspect(depth, vk::ImageAspectFlags::DEPTH)
            .write(&mut self.surfel_meta_buf)
            .write(&mut self.surfel_hash_key_buf)
            .write(&mut self.surfel_hash_value_buf)
            .read(&self.cell_index_offset_buf)
            .read(&self.surfel_index_buf)
            .read(&self.surfel_spatial_buf)
            .read(&self.surfel_irradiance_buf)
            .write(&mut tile_surfel_alloc_tex)
            .write(&mut debug_out)
            .dispatch(gbuffer.desc().extent);

        SimpleComputePass::new(
            rg.add_pass(),
            "/assets/shaders/surfel_gi/allocate_surfels.hlsl",
        )
        .read(&gbuffer)
        .read_aspect(depth, vk::ImageAspectFlags::DEPTH)
        .read(&self.surfel_meta_buf)
        .read(&self.surfel_hash_key_buf)
        .read(&self.surfel_hash_value_buf)
        .write(&mut self.cell_index_offset_buf) // hack: should be &
        .read(&self.surfel_index_buf)
        .write(&mut self.surfel_spatial_buf)
        .read(&tile_surfel_alloc_tex)
        .write(&mut debug_out)
        .dispatch(tile_surfel_alloc_tex.desc().extent);

        debug_out
    }

    pub fn trace_irradiance(
        &mut self,
        rg: &mut rg::RenderGraph,
        bindless_descriptor_set: vk::DescriptorSet,
        tlas: &rg::Handle<RayTracingAcceleration>,
    ) {
        let indirect_args_buf = {
            let mut pass = rg.add_pass();
            let mut indirect_args_buf = pass.create(&BufferDesc::new(
                size_of::<u32>() * 4,
                vk::BufferUsageFlags::SHADER_DEVICE_ADDRESS,
            ));

            SimpleComputePass::new(
                pass,
                "/assets/shaders/surfel_gi/prepare_trace_dispatch_args.hlsl",
            )
            .read(&mut self.surfel_meta_buf)
            .write(&mut indirect_args_buf)
            .dispatch([1, 1, 1]);

            indirect_args_buf
        };

        let mut pass = rg.add_pass();

        let pipeline = pass.register_ray_tracing_pipeline(
            &[
                PipelineShader {
                    code: "/assets/shaders/surfel_gi/trace_irradiance.rgen.hlsl",
                    desc: PipelineShaderDesc::builder(ShaderPipelineStage::RayGen)
                        .build()
                        .unwrap(),
                },
                PipelineShader {
                    code: "/assets/shaders/rt/triangle.rmiss.hlsl",
                    desc: PipelineShaderDesc::builder(ShaderPipelineStage::RayMiss)
                        .build()
                        .unwrap(),
                },
                PipelineShader {
                    code: "/assets/shaders/rt/shadow.rmiss.hlsl",
                    desc: PipelineShaderDesc::builder(ShaderPipelineStage::RayMiss)
                        .build()
                        .unwrap(),
                },
                PipelineShader {
                    code: "/assets/shaders/rt/triangle.rchit.hlsl",
                    desc: PipelineShaderDesc::builder(ShaderPipelineStage::RayClosestHit)
                        .build()
                        .unwrap(),
                },
            ],
            slingshot::backend::ray_tracing::RayTracingPipelineDesc::default()
                .max_pipeline_ray_recursion_depth(2),
        );

        let tlas_ref = pass.read(&tlas, AccessType::AnyShaderReadOther);

        let spatial_ref = pass.read(
            &self.surfel_spatial_buf,
            AccessType::AnyShaderReadSampledImageOrUniformTexelBuffer,
        );
        let output_ref = pass.write(&mut self.surfel_irradiance_buf, AccessType::AnyShaderWrite);
        let indirect_args_ref = pass.read(&indirect_args_buf, AccessType::IndirectBuffer);

        pass.render(move |api| {
            let pipeline = api.bind_ray_tracing_pipeline(
                pipeline
                    .into_binding()
                    .descriptor_set(0, &[spatial_ref.bind(), output_ref.bind()])
                    .raw_descriptor_set(1, bindless_descriptor_set)
                    .descriptor_set(3, &[tlas_ref.bind()]),
            );

            pipeline.trace_rays_indirect(indirect_args_ref);
            //pipeline.trace_rays(indirect_args_ref, [1000, 1, 1]);
        });
    }
}

struct SimpleComputePass<'rg> {
    pass: rg::PassBuilder<'rg>,
    pipeline: rg::RgComputePipelineHandle,
    bindings: Vec<rg::RenderPassBinding>,
}

impl<'rg> SimpleComputePass<'rg> {
    pub fn new(mut pass: rg::PassBuilder<'rg>, pipeline_path: &str) -> Self {
        let pipeline = pass.register_compute_pipeline(pipeline_path);

        Self {
            pass,
            pipeline,
            bindings: Vec::new(),
        }
    }

    pub fn read<Res>(mut self, handle: &rg::Handle<Res>) -> Self
    where
        Res: Resource + 'static,
        rg::Ref<Res, rg::GpuSrv>: rg::BindRgRef,
    {
        let handle_ref = self.pass.read(
            handle,
            AccessType::ComputeShaderReadSampledImageOrUniformTexelBuffer,
        );

        self.bindings.push(rg::BindRgRef::bind(&handle_ref));

        self
    }

    pub fn read_aspect(
        mut self,
        handle: &rg::Handle<Image>,
        aspect_mask: vk::ImageAspectFlags,
    ) -> Self {
        let handle_ref = self.pass.read(
            handle,
            AccessType::ComputeShaderReadSampledImageOrUniformTexelBuffer,
        );

        self.bindings
            .push(handle_ref.bind_view(ImageViewDescBuilder::default().aspect_mask(aspect_mask)));

        self
    }

    pub fn write<Res>(mut self, handle: &mut rg::Handle<Res>) -> Self
    where
        Res: Resource + 'static,
        rg::Ref<Res, rg::GpuUav>: rg::BindRgRef,
    {
        let handle_ref = self.pass.write(handle, AccessType::ComputeShaderWrite);

        self.bindings.push(rg::BindRgRef::bind(&handle_ref));

        self
    }

    pub fn dispatch(self, extent: [u32; 3]) {
        let pipeline = self.pipeline;
        let bindings = self.bindings;

        self.pass.render(move |api| {
            let pipeline =
                api.bind_compute_pipeline(pipeline.into_binding().descriptor_set(0, &bindings));

            pipeline.dispatch(extent);
        });
    }
}