#[allow(clippy::all)]
pub(in crate) mod gl;
pub mod surface;
pub mod gpu_program;
pub mod error;

mod geometry_buffer;
mod ui_renderer;
mod particle_system_renderer;
mod gbuffer;
mod deferred_light_renderer;
mod shadow_map_renderer;
mod flat_shader;
pub mod gpu_texture;
mod sprite_renderer;

use std::time;
use rg3d_core::{
    math::{vec3::Vec3, mat4::Mat4},
    color::Color,
};
use glutin::PossiblyCurrent;
use crate::{
    engine::resource_manager::ResourceManager,
    gui::draw::DrawingContext,
    renderer::{
        ui_renderer::UIRenderer,
        surface::SurfaceSharedData,
        particle_system_renderer::ParticleSystemRenderer,
        gbuffer::GBuffer,
        deferred_light_renderer::DeferredLightRenderer,
        error::RendererError,
        gpu_texture::{GpuTexture, GpuTextureKind, PixelKind},
        flat_shader::FlatShader,
        sprite_renderer::SpriteRenderer,
    },
    scene::{
        SceneInterface,
        SceneContainer,
        node::Node,
    },
};

#[repr(C)]
pub struct TriangleDefinition {
    pub a: u32,
    pub b: u32,
    pub c: u32,
}

fn check_gl_error_internal(line: u32, file: &str) {
    unsafe {
        let error_code = gl::GetError();
        if error_code != gl::NO_ERROR {
            match error_code {
                gl::INVALID_ENUM => print!("GL_INVALID_ENUM"),
                gl::INVALID_VALUE => print!("GL_INVALID_VALUE"),
                gl::INVALID_OPERATION => print!("GL_INVALID_OPERATION"),
                gl::STACK_OVERFLOW => print!("GL_STACK_OVERFLOW"),
                gl::STACK_UNDERFLOW => print!("GL_STACK_UNDERFLOW"),
                gl::OUT_OF_MEMORY => print!("GL_OUT_OF_MEMORY"),
                _ => (),
            };

            println!(" error has occurred! At line {} in file {}, stability is not guaranteed!", line, file);
        }
    }
}

macro_rules! check_gl_error {
    () => (check_gl_error_internal(line!(), file!()))
}

#[derive(Copy, Clone)]
pub struct Statistics {
    /// Real time consumed to render frame.
    pub pure_frame_time: f32,
    /// Total time renderer took to process single frame, usually includes
    /// time renderer spend to wait to buffers swap (can include vsync)
    pub capped_frame_time: f32,
    /// Total amount of frames been rendered in one second.
    pub frames_per_second: usize,
    frame_counter: usize,
    frame_start_time: time::Instant,
    last_fps_commit_time: time::Instant,
}

impl Statistics {
    /// Must be called before render anything.
    fn begin_frame(&mut self) {
        self.frame_start_time = time::Instant::now();
    }

    /// Must be called before SwapBuffers but after all rendering is done.
    fn end_frame(&mut self) {
        let current_time = time::Instant::now();

        self.pure_frame_time = current_time.duration_since(self.frame_start_time).as_secs_f32();
        self.frame_counter += 1;

        if current_time.duration_since(self.last_fps_commit_time).as_secs_f32() >= 1.0 {
            self.last_fps_commit_time = current_time;
            self.frames_per_second = self.frame_counter;
            self.frame_counter = 0;
        }
    }

    /// Must be called after SwapBuffers to get capped frame time.
    fn finalize(&mut self) {
        self.capped_frame_time = time::Instant::now().duration_since(self.frame_start_time).as_secs_f32();
    }
}

impl Default for Statistics {
    fn default() -> Self {
        Self {
            pure_frame_time: 0.0,
            capped_frame_time: 0.0,
            frames_per_second: 0,
            frame_counter: 0,
            frame_start_time: time::Instant::now(),
            last_fps_commit_time: time::Instant::now(),
        }
    }
}

pub struct Renderer {
    deferred_light_renderer: DeferredLightRenderer,
    gbuffer: GBuffer,
    flat_shader: FlatShader,
    sprite_renderer: SpriteRenderer,
    particle_system_renderer: ParticleSystemRenderer,
    /// Dummy white one pixel texture which will be used as stub when rendering
    /// something without texture specified.
    white_dummy: GpuTexture,
    /// Dummy one pixel texture with (0, 1, 0) vector is used as stub when rendering
    /// something without normal map.
    normal_dummy: GpuTexture,
    ui_renderer: UIRenderer,
    statistics: Statistics,
    quad: SurfaceSharedData,
    last_render_time: time::Instant,
    frame_size: (u32, u32),
    ambient_color: Color,
}

impl Renderer {
    pub(in crate) fn new(frame_size: (u32, u32)) -> Result<Self, RendererError> {
        unsafe {
            gl::Enable(gl::DEPTH_TEST);
        }

        Ok(Self {
            frame_size,
            deferred_light_renderer: DeferredLightRenderer::new()?,
            flat_shader: FlatShader::new()?,
            gbuffer: GBuffer::new(frame_size)?,
            statistics: Statistics::default(),
            sprite_renderer: SpriteRenderer::new()?,
            white_dummy: GpuTexture::new(GpuTextureKind::Rectangle { width: 1, height: 1 },
                                         PixelKind::RGBA8, &[255, 255, 255, 255],
                                         false)?,
            normal_dummy: GpuTexture::new(GpuTextureKind::Rectangle { width: 1, height: 1 },
                                          PixelKind::RGBA8, &[128, 128, 255, 255],
                                          false)?,
            quad: SurfaceSharedData::make_unit_xy_quad(),
            ui_renderer: UIRenderer::new()?,
            particle_system_renderer: ParticleSystemRenderer::new()?,
            last_render_time: time::Instant::now(), // TODO: Is this right?
            ambient_color: Color::opaque(100, 100, 100),
        })
    }

    pub fn set_ambient_color(&mut self, color: Color) {
        self.ambient_color = color;
    }

    pub fn get_ambient_color(&self) -> Color {
        self.ambient_color
    }

    pub fn get_statistics(&self) -> Statistics {
        self.statistics
    }

    pub fn upload_resources(&mut self, resource_manager: &mut ResourceManager) {
        for texture_rc in resource_manager.get_textures() {
            let mut texture = texture_rc.lock().unwrap();
            if texture.gpu_tex.is_none() {
                let gpu_texture = GpuTexture::new(
                    GpuTextureKind::Rectangle { width: texture.width as usize, height: texture.height as usize },
                    PixelKind::from(texture.kind), texture.bytes.as_slice(), true).unwrap();
                gpu_texture.set_max_anisotropy();
                texture.gpu_tex = Some(gpu_texture);
            }
        }
    }

    /// Sets new frame size, should be called when received a Resize event.
    pub fn set_frame_size(&mut self, new_size: (u32, u32)) -> Result<(), RendererError> {
        self.frame_size = new_size;
        self.gbuffer = GBuffer::new(new_size)?;
        Ok(())
    }

    pub fn get_frame_size(&self) -> (u32, u32) {
        self.frame_size
    }

    pub(in crate) fn render(&mut self, scenes: &SceneContainer, drawing_context: &DrawingContext,
                            context: &glutin::WindowedContext<PossiblyCurrent>) -> Result<(), RendererError> {
        self.statistics.begin_frame();

        let frame_width = self.frame_size.0 as f32;
        let frame_height = self.frame_size.1 as f32;
        let frame_matrix =
            Mat4::ortho(0.0, frame_width, frame_height, 0.0, -1.0, 1.0) *
                Mat4::scale(Vec3::new(frame_width, frame_height, 0.0));

        // Render scenes into g-buffer.
        for scene in scenes.iter() {
            let SceneInterface { graph, .. } = scene.interface();

            // Prepare for render - fill lists of nodes participating in rendering.
            let camera = match graph.linear_iter().find(|node| node.is_camera()) {
                Some(camera) => camera,
                None => continue
            };

            let camera = match camera {
                Node::Camera(camera) => camera,
                _ => continue
            };

            self.gbuffer.fill(
                frame_width,
                frame_height,
                graph,
                camera,
                &self.white_dummy,
                &self.normal_dummy,
            );

            self.deferred_light_renderer.render(
                frame_width,
                frame_height,
                scene,
                camera,
                &self.gbuffer,
                &self.white_dummy,
                self.ambient_color,
            );
        }

        self.particle_system_renderer.render(
            scenes,
            &self.white_dummy,
            frame_width,
            frame_height,
            &self.gbuffer,
        );

        self.sprite_renderer.render(scenes, &self.white_dummy);

        unsafe {
            // Finally render everything into back buffer.
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::Viewport(0, 0, frame_width as i32, frame_height as i32);
            gl::StencilMask(0xFF);
            gl::DepthMask(gl::TRUE);
            gl::ColorMask(gl::TRUE, gl::TRUE, gl::TRUE, gl::TRUE);
            gl::Clear(gl::COLOR_BUFFER_BIT | gl::DEPTH_BUFFER_BIT | gl::STENCIL_BUFFER_BIT);
        }
        self.flat_shader.bind();
        self.flat_shader.set_wvp_matrix(&frame_matrix);
        self.flat_shader.set_diffuse_texture(0);

        unsafe {
            gl::ActiveTexture(gl::TEXTURE0);
            gl::BindTexture(gl::TEXTURE_2D, self.gbuffer.frame_texture);
        }
        self.quad.draw();

        self.ui_renderer.render(
            frame_width,
            frame_height,
            drawing_context,
            &self.white_dummy,
        )?;

        self.statistics.end_frame();

        if context.swap_buffers().is_err() {
            println!("Failed to swap buffers!");
        }

        check_gl_error!();

        self.statistics.finalize();

        Ok(())
    }
}