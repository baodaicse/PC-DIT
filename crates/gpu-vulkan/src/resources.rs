//! Owners used during partial construction and for persistent sequential reuse.
//! The backend drops these before destroying the logical device.
use crate::Buffer;
use ash::vk;

pub(crate) struct MatrixBuffers {
    pub device: ash::Device,
    pub buffers: Vec<Buffer>,
}

impl Drop for MatrixBuffers {
    fn drop(&mut self) {
        for buffer in self.buffers.drain(..).rev() {
            unsafe {
                self.device.destroy_buffer(buffer.handle, None);
                self.device.free_memory(buffer.memory, None);
            }
        }
    }
}

pub(crate) struct MatrixPipeline {
    pub device: ash::Device,
    pub set_layout: vk::DescriptorSetLayout,
    pub pipeline_layout: vk::PipelineLayout,
    pub shader: vk::ShaderModule,
    pub pipeline: vk::Pipeline,
    pub descriptor_pool: vk::DescriptorPool,
    pub set: vk::DescriptorSet,
}

impl MatrixPipeline {
    pub fn new(device: ash::Device) -> Self {
        Self {
            device,
            set_layout: vk::DescriptorSetLayout::null(),
            pipeline_layout: vk::PipelineLayout::null(),
            shader: vk::ShaderModule::null(),
            pipeline: vk::Pipeline::null(),
            descriptor_pool: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
        }
    }
}

impl Drop for MatrixPipeline {
    fn drop(&mut self) {
        // Vulkan destroy calls accept null handles, including partial setup.
        unsafe {
            self.device
                .destroy_descriptor_pool(self.descriptor_pool, None);
            self.device.destroy_pipeline(self.pipeline, None);
            self.device.destroy_shader_module(self.shader, None);
            self.device
                .destroy_pipeline_layout(self.pipeline_layout, None);
            self.device
                .destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}
