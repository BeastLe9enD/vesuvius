pub mod config;
pub mod shader;

use std::{fs, slice};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use ash::vk;
use log::info;
use render::pipeline::config::PipelineConfiguration;
use render::pipeline::shader::{ShaderKind, ShaderModule};
use crate::App;
use crate::Result;

/// This structure represents a render pipeline. The complete pipeline is re-compilable, when the
/// source code or the configuration file changes. The re-compilation feature is used by the file
/// watcher in the Game Renderer.
#[derive(Clone)]
pub struct RenderPipeline {
    shader_modules: Vec<ShaderModule>,
    application: App,
    vulkan_pipeline_layout: Option<vk::PipelineLayout>,
    pub(crate) vulkan_pipeline: Option<vk::Pipeline>,
    name: String
}

impl Drop for RenderPipeline {
    fn drop(&mut self) {
        let device = self.application.main_device().virtual_device();
        if let Some(vulkan_pipeline_layout) = self.vulkan_pipeline_layout {
            unsafe { device.destroy_pipeline_layout(vulkan_pipeline_layout, None) };
        }

        if let Some(vulkan_pipeline) = self.vulkan_pipeline {
            unsafe { device.destroy_pipeline(vulkan_pipeline, None) };
        }
    }
}

impl RenderPipeline {

    pub fn new<P: AsRef<Path>>(application: App, path: P) -> Result<Self> {
        let path = path.as_ref();
        if !path.is_file() {
            panic!("Unable to create render pipeline => The path '{}' doesn't points to a file",
                   path.to_str().unwrap());
        }

        // Read configuration from file
        let file_content = String::from_utf8(fs::read(path)?)?;
        let pipeline_configuration = serde_json::from_str::<PipelineConfiguration>(&file_content)
            .expect("Illegal pipeline configuration file specified");

        // Create shader from file
        let mut shader_modules = Vec::new();
        for shader_configuration in pipeline_configuration.shader.iter() {
            // Get shader path and validate
            let shader_path = PathBuf::from_str(&shader_configuration.resource).unwrap();
            if !shader_path.exists() || !shader_path.is_file() {
                panic!("Unable to create shader module => The path '{}' doesn't points to a file",
                       shader_path.to_str().unwrap());
            }

            // Push shader into list
            shader_modules.push(ShaderModule {
                application: application.clone(),
                shader_source_path: shader_path,
                vulkan_shader_module: None,
                kind: shader_configuration.kind,
                shader_ir_code: Vec::new()
            })
        }
        info!("Internally created '{}' render pipeline with {} shaders",
            pipeline_configuration.name, shader_modules.len());

        Ok(Self {
            application,
            shader_modules,
            vulkan_pipeline_layout: None,
            vulkan_pipeline: None,
            name: pipeline_configuration.name
        })
    }

    pub fn compile(&mut self) -> Result<()> {
        let window_size = self.application.window().inner_size();
        let device = self.application.main_device().virtual_device();

        for shader in self.shader_modules.iter_mut() {
            shader.compile()?;
        }

        // Viewport and scissor
        let viewport = vk::Viewport::default().x(0.0).y(0.0)
            .width(window_size.width as f32)
            .height(window_size.height as f32)
            .min_depth(0.0)
            .max_depth(1.0);
        let scissor = vk::Rect2D::default().extent(vk::Extent2D {
            width: window_size.width,
            height: window_size.height
        });
        let viewport_state_create_info = vk::PipelineViewportStateCreateInfo::default()
            .scissors(slice::from_ref(&scissor))
            .viewports(slice::from_ref(&viewport));


        // Some stage infos
        let rasterization_stage_create_info = vk::PipelineRasterizationStateCreateInfo::default()
            .rasterizer_discard_enable(false)
            .depth_clamp_enable(false)
            .polygon_mode(vk::PolygonMode::FILL) // TODO: Read from config
            .cull_mode(vk::CullModeFlags::NONE)
            .front_face(vk::FrontFace::CLOCKWISE)
            .depth_bias_enable(false)
            .line_width(1.0);
        let multisample_stage_create_info = vk::PipelineMultisampleStateCreateInfo::default()
            .rasterization_samples(vk::SampleCountFlags::TYPE_1)
            .sample_shading_enable(false)
            .alpha_to_coverage_enable(false)
            .alpha_to_one_enable(false);

        // Color Blend infos
        let pipeline_color_blend_attachment_info = vk::PipelineColorBlendAttachmentState::default()
            .color_write_mask(vk::ColorComponentFlags::RGBA);
        let pipeline_color_blend_state_create_info = vk::PipelineColorBlendStateCreateInfo::default()
            .attachments(slice::from_ref(&pipeline_color_blend_attachment_info));

        // Create pipeline layout
        let layout_create_info = vk::PipelineLayoutCreateInfo::default();
        let layout = unsafe { device.create_pipeline_layout(&layout_create_info, None) }?;

        // Create pipeline with recompiled shader modules
        let mut pipeline_rendering_create_info = vk::PipelineRenderingCreateInfo::default()
            .color_attachment_formats(&[vk::Format::B8G8R8A8_UNORM]);
        let dynamic_state_create_info = vk::PipelineDynamicStateCreateInfo::default();
        let input_assembly_state_create_info = vk::PipelineInputAssemblyStateCreateInfo::default()
            .topology(vk::PrimitiveTopology::TRIANGLE_LIST) // Weather draw the stuff as triangles, lines etc.
            .primitive_restart_enable(false); // Ignore lol

        // Configure pipeline input state
        let vertex_shader = self.shader_modules.iter()
            .find(|module| module.kind == ShaderKind::Vertex).unwrap();
        let (input_attrs, binding_desc) = vertex_shader.reflect_input_attributes();

        let vertex_input_state_create_info = vk::PipelineVertexInputStateCreateInfo::default()
            .vertex_attribute_descriptions(input_attrs.as_slice())
            .vertex_binding_descriptions(slice::from_ref(&binding_desc));

        // Create pipeline with recompiled shader modules
        let stages = self.shader_modules.iter()
            .map(|module| module.into())
            .collect::<Vec<_>>();

        let graphics_pipeline_create_info = vk::GraphicsPipelineCreateInfo::default()
            .push_next(&mut pipeline_rendering_create_info)
            .vertex_input_state(&vertex_input_state_create_info)
            .input_assembly_state(&input_assembly_state_create_info)
            .color_blend_state(&pipeline_color_blend_state_create_info)
            .rasterization_state(&rasterization_stage_create_info)
            .multisample_state(&multisample_stage_create_info)
            .viewport_state(&viewport_state_create_info)
            .dynamic_state(&dynamic_state_create_info)
            .stages(stages.as_slice())
            .base_pipeline_handle(vk::Pipeline::null())
            .layout(layout);

        // Destroy old handles in memory
        if let Some(old_pipeline) = self.vulkan_pipeline {
            unsafe { device.destroy_pipeline(old_pipeline, None) };
        }

        if let Some(old_layout_handle) = self.vulkan_pipeline_layout {
            unsafe { device.destroy_pipeline_layout(old_layout_handle, None) };
        }


        // Replace old handles with new handles
        self.vulkan_pipeline_layout = Some(layout);
        self.vulkan_pipeline = Some(unsafe {
            device.create_graphics_pipelines(
                vk::PipelineCache::null(),
                slice::from_ref(&graphics_pipeline_create_info),
                None
            )
        }.unwrap()[0]);
        Ok(())
    }

}