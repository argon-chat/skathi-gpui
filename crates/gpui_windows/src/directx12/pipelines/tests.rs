use super::*;
use windows::{Win32::Graphics::Direct3D::Fxc::D3DReflect, core::Interface};

fn modules() -> impl Iterator<Item = ShaderModule> {
    [
        ShaderModule::Quad,
        ShaderModule::Shadow,
        ShaderModule::BlurDownsample,
        ShaderModule::BlurUpsample,
        ShaderModule::BlurRect,
        ShaderModule::Underline,
        ShaderModule::PathTile,
        ShaderModule::MonochromeSprite,
        ShaderModule::SubpixelSprite,
        ShaderModule::PolychromeSprite,
        ShaderModule::VnLayer,
        ShaderModule::VnUnderlay,
    ]
    .into_iter()
    .chain(EffectShader::ALL.into_iter().map(ShaderModule::Effect))
}

fn contract(bytecode: &[u8], vertex: bool) -> Result<String> {
    ensure!(
        bytecode.starts_with(b"DXBC"),
        "native D3D12 shader must contain DXBC bytecode"
    );
    let mut raw = std::ptr::null_mut();
    unsafe {
        D3DReflect(
            bytecode.as_ptr().cast(),
            bytecode.len(),
            &ID3D12ShaderReflection::IID,
            &mut raw,
        )
    }?;
    let reflection = unsafe { ID3D12ShaderReflection::from_raw(raw) };
    let mut description = D3D12_SHADER_DESC::default();
    unsafe { reflection.GetDesc(&mut description) }?;
    ensure!(
        description.Version & 0xff == 0x51,
        "native shader must target model 5.1: {:#x}",
        description.Version
    );
    ensure!(
        description.Version >> 16 == u32::from(vertex),
        "native shader stage is incorrect"
    );
    let mut fields = vec![format!(
        "version={} flags={}",
        description.Version, description.Flags
    )];
    for index in 0..description.BoundResources {
        let mut resource = D3D12_SHADER_INPUT_BIND_DESC::default();
        unsafe { reflection.GetResourceBindingDesc(index, &mut resource) }?;
        let name = unsafe { resource.Name.to_string() }?;
        resource.Name = PCSTR::null();
        fields.push(format!("resource {name}: {resource:?}"));
    }
    for (input, count) in [
        (true, description.InputParameters),
        (false, description.OutputParameters),
    ] {
        for index in 0..count {
            let mut parameter = D3D12_SIGNATURE_PARAMETER_DESC::default();
            unsafe {
                if input {
                    reflection.GetInputParameterDesc(index, &mut parameter)
                } else {
                    reflection.GetOutputParameterDesc(index, &mut parameter)
                }
            }?;
            let name = unsafe { parameter.SemanticName.to_string() }?;
            parameter.SemanticName = PCSTR::null();
            fields.push(format!("signature input={input} {name}: {parameter:?}"));
        }
    }
    for index in 0..description.ConstantBuffers {
        let buffer = unsafe { reflection.GetConstantBufferByIndex(index) }
            .context("shader constant buffer missing")?;
        let mut buffer_description = D3D12_SHADER_BUFFER_DESC::default();
        unsafe { buffer.GetDesc(&mut buffer_description) }?;
        let name = unsafe { buffer_description.Name.to_string() }?;
        fields.push(format!(
            "buffer {name}: size={} type={:?} flags={}",
            buffer_description.Size, buffer_description.Type, buffer_description.uFlags
        ));
        for index in 0..buffer_description.Variables {
            let variable = unsafe { buffer.GetVariableByIndex(index) }
                .context("shader constant variable missing")?;
            let mut variable_description = D3D12_SHADER_VARIABLE_DESC::default();
            unsafe { variable.GetDesc(&mut variable_description) }?;
            let variable_name = unsafe { variable_description.Name.to_string() }?;
            fields.push(format!("variable {name}/{variable_name}: offset={} size={} flags={} textures={}:{} samplers={}:{}",
                variable_description.StartOffset, variable_description.Size, variable_description.uFlags,
                variable_description.StartTexture, variable_description.TextureSize,
                variable_description.StartSampler, variable_description.SamplerSize));
        }
    }
    fields.sort();
    Ok(fields.join("\n"))
}

#[test]
fn embedded_shader_contracts_match_current_sm51_hlsl() -> Result<()> {
    for module in modules() {
        let (expected_vertex, expected_fragment) =
            compile_module(module).with_context(|| format!("reference D3D12 shader {module:?}"))?;
        let (vertex, fragment) = embedded::module(module);
        for (artifact, reference, is_vertex) in [
            (vertex, expected_vertex.as_slice(), true),
            (fragment, expected_fragment.as_slice(), false),
        ] {
            assert_eq!(
                contract(artifact, is_vertex)?,
                contract(reference, is_vertex)?,
                "embedded {module:?} vertex={is_vertex} must preserve resource bindings, signatures and constant layouts"
            );
        }
    }
    Ok(())
}

#[test]
fn embedded_artifacts_track_current_sources_and_transitive_includes() -> Result<()> {
    for (path, expected_hash) in embedded::INPUTS {
        let source = std::fs::read(path).with_context(|| format!("shader build input {path}"))?;
        let hash = source.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
        });
        assert_eq!(
            hash, *expected_hash,
            "embedded shader artifacts must be rebuilt after {path} changes"
        );
    }
    assert!(
        embedded::INPUTS
            .iter()
            .any(|(path, _)| path.ends_with("alpha_correction.hlsl")),
        "transitive shader includes must participate in rebuild tracking"
    );
    Ok(())
}

#[test]
#[ignore = "requires a native D3D12 hardware adapter; run explicitly on Windows"]
fn native_prewarmed_pipelines_serve_their_first_use() -> Result<()> {
    let device = super::super::device::Device::new()?;
    let mut pipelines = Pipelines::new(&device.device)?;
    let format = windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    pipelines.prewarm(format);
    let deadline = Instant::now() + std::time::Duration::from_secs(60);
    let primitives = |pipelines: &Pipelines| {
        let warmed = pipelines.warmed.lock();
        let primitive =
            |cached: &&CachedPipeline| matches!(cached.key.kind, PipelineKind::Primitive(_));
        warmed.iter().filter(primitive).count()
    };
    while primitives(&pipelines) < WARM_MODULES.len() {
        ensure!(Instant::now() < deadline, "pipeline warmup did not finish");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    for (module, samples) in WARM_MODULES {
        let warmed = pipelines
            .warmed
            .lock()
            .iter()
            .find(|cached| {
                cached.key.kind == PipelineKind::Primitive(module) && cached.key.samples == samples
            })
            .map(|cached| cached.state.as_raw())
            .context("warmed pipeline missing")?;
        let served = pipelines.pipeline(module, format, samples)?;
        assert_eq!(
            served.as_raw(),
            warmed,
            "{module:?} must come from the warmup"
        );
    }
    assert_eq!(pipelines.cache.len(), WARM_MODULES.len());
    Ok(())
}

#[test]
#[ignore = "requires a native D3D12 hardware adapter; run explicitly on Windows"]
fn native_app_effects_are_warmed_before_their_first_frame_and_after_a_reload() -> Result<()> {
    let install = |rgb: &str| -> Result<()> {
        gpui::reload_app_effects(
            gpui::AppEffects::new(
                "test",
                r#"{"schema": 14, "effects": [
                    {"name": "tint", "backdrop": false, "history": true, "hlsl": "tint.hlsl"}
                ]}"#,
                [(
                    "tint.hlsl".to_string(),
                    format!(
                        "float4 effect_fragment() : SV_Target {{ return float4({rgb}, 1.0); }}"
                    ),
                )],
            )
            .map_err(anyhow::Error::msg)?,
        );
        Ok(())
    };
    let warmed = |pipelines: &Pipelines, kind: fn(u64) -> PipelineKind, format| {
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        loop {
            let live = pipelines
                .app_warmup
                .is_idle()
                .then(|| pipelines.app_warm.lock().fragments.last().map(|f| f.id))
                .flatten();
            let found = live.and_then(|id| {
                pipelines
                    .warmed
                    .lock()
                    .iter()
                    .find(|cached| cached.key.kind == kind(id) && cached.key.format == format)
                    .map(|cached| cached.state.as_raw())
            });
            if let Some(found) = found {
                return Ok(found);
            }
            ensure!(
                Instant::now() < deadline,
                "app effect warmup did not finish"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };
    let _prewarm = Prewarm::set(gpui::AppEffectsPrewarm::All);
    let device = super::super::device::Device::new()?;
    let mut pipelines = Pipelines::new(&device.device)?;
    let format = windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    let index = EffectShader::ALL.len() as u32;
    install("1.0, 0.0, 0.0")?;
    pipelines.prewarm(format);
    let first = warmed(&pipelines, PipelineKind::AppEffect, format)?;
    let history = warmed(&pipelines, PipelineKind::AppHistory, HISTORY_FORMAT)?;
    let (served, _) = pipelines
        .effect(index, format, 1)?
        .context("app effect missing")?;
    assert_eq!(
        served.as_raw(),
        first,
        "the first draw must take the warmed pipeline"
    );
    let served = pipelines.history(index)?.context("history missing")?;
    assert_eq!(served.as_raw(), history);

    install("0.0, 1.0, 0.0")?;
    pipelines.warm_app_effects();
    let reloaded = warmed(&pipelines, PipelineKind::AppEffect, format)?;
    assert_ne!(reloaded, first);
    let (served, _) = pipelines
        .effect(index, format, 1)?
        .context("app effect missing")?;
    assert_eq!(served.as_raw(), reloaded, "a reload must be warmed again");

    let hdr = DXGI_FORMAT_R16G16B16A16_FLOAT;
    pipelines.prewarm(hdr);
    let hdr_warmed = warmed(&pipelines, PipelineKind::AppEffect, hdr)?;
    let (served, _) = pipelines
        .effect(index, hdr, 1)?
        .context("app effect missing")?;
    assert_eq!(
        served.as_raw(),
        hdr_warmed,
        "a new format warms the synced effects"
    );
    assert_eq!(pipelines.app_fragments_compiled_on_frame, 0);
    Ok(())
}

/// The prewarm setting is process-wide; back to the default when a test ends.
struct Prewarm;

impl Prewarm {
    fn set(prewarm: gpui::AppEffectsPrewarm) -> Self {
        gpui::set_app_effects_prewarm(prewarm);
        Self
    }
}

impl Drop for Prewarm {
    fn drop(&mut self) {
        gpui::set_app_effects_prewarm(gpui::AppEffectsPrewarm::Off);
    }
}

#[test]
#[ignore = "requires a native D3D12 hardware adapter; run explicitly on Windows"]
fn native_app_effects_are_warmed_only_as_the_prewarm_setting_says() -> Result<()> {
    let solid = |rgb: &str| {
        format!("float4 effect_fragment() : SV_Target {{ return float4({rgb}, 1.0); }}")
    };
    let format = windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    let index = EffectShader::ALL.len() as u32;
    for (prewarm, warmed, on_frame) in [
        (gpui::AppEffectsPrewarm::Off, 0, 2),
        (
            gpui::AppEffectsPrewarm::Only(vec!["other".into(), "missing".into()]),
            1,
            1,
        ),
    ] {
        let _prewarm = Prewarm::set(prewarm);
        gpui::reload_app_effects(
            gpui::AppEffects::new(
                "test",
                r#"{"schema": 14, "effects": [
                    {"name": "tint", "backdrop": false, "hlsl": "tint.hlsl"},
                    {"name": "other", "backdrop": false, "hlsl": "other.hlsl"}
                ]}"#,
                [
                    ("tint.hlsl".to_string(), solid("1.0, 0.0, 0.0")),
                    ("other.hlsl".to_string(), solid("0.0, 1.0, 0.0")),
                ],
            )
            .map_err(anyhow::Error::msg)?,
        );
        let device = super::super::device::Device::new()?;
        let mut pipelines = Pipelines::new(&device.device)?;
        pipelines.prewarm(format);
        let deadline = Instant::now() + std::time::Duration::from_secs(60);
        while !pipelines.app_warmup.is_idle() {
            ensure!(
                Instant::now() < deadline,
                "app effect warmup did not finish"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(pipelines.app_warm.lock().fragments.len(), warmed);
        pipelines
            .effect(index, format, 1)?
            .context("app effect missing")?;
        assert_eq!(pipelines.app_fragments_compiled_on_frame, on_frame);
        pipelines.prewarm(DXGI_FORMAT_R16G16B16A16_FLOAT);
        assert_eq!(
            pipelines.app_warm.lock().fragments.len(),
            warmed,
            "an effect built on its frame is not warmed for another format"
        );
    }
    Ok(())
}
