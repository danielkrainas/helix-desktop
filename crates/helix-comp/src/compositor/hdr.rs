//! HDR monitors (helix-desktop-spec `org.gnome.Mutter.DisplayConfig.md`,
//! "Color modes"): a monitor in the BT.2100 color mode is drawn as usual
//! into an offscreen texture, which is then drawn through a shader that
//! converts it to PQ with BT.2020 primaries, SDR white at the monitor's
//! `output-luminance`.
//!
//! Not yet tried on an HDR monitor; the pass itself can be seen in a
//! nested session with `HELIX_TEST_HDR=1` (the window then shows the PQ
//! encoding as if it were sRGB: washed out).

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::{Id, Kind, RenderElement};
use smithay::backend::renderer::gles::element::TextureShaderElement;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType};
use smithay::backend::renderer::{Bind, Offscreen, Renderer};
use smithay::output::Output;
use smithay::utils::{Physical, Size, Transform};

/// The pass for one monitor: its shader and offscreen texture.
#[derive(Default)]
pub struct HdrPass {
    program: Option<GlesTexProgram>,
    target: Option<(GlesTexture, Size<i32, Physical>, OutputDamageTracker)>,
}

impl std::fmt::Debug for HdrPass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HdrPass").field("size", &self.target.as_ref().map(|t| t.1)).finish()
    }
}

impl HdrPass {
    /// Draw `elements` (as they would go to `output`) offscreen, and give
    /// the element that shows them converted, SDR white at `white`
    /// cd/m².
    pub fn convert<E: RenderElement<GlesRenderer>>(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        elements: &[E],
        clear: [f32; 4],
        white: f64,
    ) -> Result<TextureShaderElement, String> {
        let program = match &self.program {
            Some(program) => program.clone(),
            None => {
                let mut source = helix_comp::colorspace::hdr_shader();
                if std::env::var("HELIX_TEST_HDR").as_deref() == Ok("raw") {
                    // Debugging: the frame as drawn, without converting it.
                    source = source.replace("color = vec4(pq_encode(linear * sdr_white), 1.0);", "color = vec4(rgb, 1.0);");
                }
                if std::env::var("HELIX_TEST_HDR").as_deref() == Ok("red") {
                    source = source.replace("color = vec4(pq_encode(linear * sdr_white), 1.0);", "color = vec4(1.0, 0.0, 0.0, 1.0);");
                }
                let program = renderer
                    .compile_custom_texture_shader(source, &[UniformName::new("sdr_white", UniformType::_1f)])
                    .map_err(|e| format!("HDR shader: {e:?}"))?;
                self.program = Some(program.clone());
                program
            }
        };
        let mode = output.current_mode().ok_or("the monitor has no mode")?;
        // Elements are laid out before the monitor's transform.
        let size = output.current_transform().transform_size(mode.size);
        if self.target.as_ref().map(|t| t.1) != Some(size) {
            let texture: GlesTexture = renderer
                .create_buffer(Fourcc::Abgr8888, size.to_logical(1).to_buffer(1, Transform::Normal))
                .map_err(|e| format!("HDR texture: {e:?}"))?;
            let scale = output.current_scale().fractional_scale();
            self.target = Some((texture, size, OutputDamageTracker::new(size, scale, Transform::Normal)));
        }
        let (texture, _, tracker) = self.target.as_mut().expect("just made");
        {
            let mut framebuffer = renderer.bind(texture).map_err(|e| format!("HDR bind: {e:?}"))?;
            tracker.render_output(renderer, &mut framebuffer, 0, elements, clear).map_err(|e| format!("HDR render: {e:?}"))?;
        }
        // Leave no offscreen framebuffer bound for whatever is drawn next.
        renderer
            .with_context(|gl| unsafe { gl.BindFramebuffer(smithay::backend::renderer::gles::ffi::FRAMEBUFFER, 0) })
            .map_err(|e| format!("HDR unbind: {e:?}"))?;
        let element = TextureRenderElement::from_static_texture(
            Id::new(),
            renderer.context_id(),
            (0.0, 0.0),
            texture.clone(),
            1,
            Transform::Normal,
            None,
            None,
            None,
            None,
            Kind::Unspecified,
        );
        let white = (white / helix_comp::colorspace::PQ_PEAK) as f32;
        Ok(TextureShaderElement::new(element, program, vec![Uniform::new("sdr_white", white)]))
    }
}
