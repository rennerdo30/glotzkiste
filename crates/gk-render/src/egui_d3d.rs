//! A small egui renderer for D3D11: one dynamic vertex and index buffer for
//! the whole UI (uploaded only when a new UI frame arrives), gamma-space
//! premultiplied blending (egui's look), one scissor per primitive, and user
//! textures resolved through a callback (streams and animations).

use egui::epaint::Primitive;
use egui::{ClippedPrimitive, TextureId};
use std::collections::HashMap;
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Foundation::RECT;
use windows::core::{PCSTR, s};

const SHADER: &str = r#"
cbuffer C : register(b0) { float2 screen_points; float2 pad; };
struct VI { float2 pos : POSITION; float2 uv : TEXCOORD0; float4 color : COLOR0; };
struct VO { float4 pos : SV_Position; float2 uv : TEXCOORD0; float4 color : COLOR0; };
VO vs(VI i) {
    VO o;
    o.pos = float4(i.pos.x / screen_points.x * 2.0 - 1.0, 1.0 - i.pos.y / screen_points.y * 2.0, 0.0, 1.0);
    o.uv = i.uv;
    o.color = i.color;
    return o;
}
Texture2D tex : register(t0);
SamplerState smp : register(s0);
float4 ps(VO i) : SV_Target { return tex.Sample(smp, i.uv) * i.color; }
"#;

pub(crate) fn compile(src: &str, entry: PCSTR, target: PCSTR) -> Result<Vec<u8>, String> {
    let mut blob = None;
    let mut errors = None;
    // SAFETY: source pointer/length describe `src`; outputs are locals.
    let result = unsafe {
        D3DCompile(src.as_ptr().cast(), src.len(), s!("gk"), None, None, entry, target, 0, 0, &mut blob, Some(&mut errors))
    };
    if let Err(e) = result {
        let detail = errors
            .map(|b: ID3DBlob| unsafe {
                String::from_utf8_lossy(std::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()))
                    .into_owned()
            })
            .unwrap_or_default();
        return Err(format!("shader compile failed: {e} {detail}"));
    }
    let blob = blob.ok_or("shader compile returned no code")?;
    // SAFETY: the blob owns this many bytes.
    Ok(unsafe { std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) }.to_vec())
}

struct Tex {
    texture: ID3D11Texture2D,
    view: ID3D11ShaderResourceView,
    size: [usize; 2],
    nearest: bool,
}

struct Draw {
    texture: TextureId,
    scissor: RECT,
    index_start: u32,
    index_count: u32,
    base_vertex: i32,
}

pub(crate) struct EguiRenderer {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    layout: ID3D11InputLayout,
    linear: ID3D11SamplerState,
    nearest: ID3D11SamplerState,
    blend: ID3D11BlendState,
    raster: ID3D11RasterizerState,
    constants: ID3D11Buffer,
    vb: Option<(ID3D11Buffer, usize)>,
    ib: Option<(ID3D11Buffer, usize)>,
    textures: HashMap<TextureId, Tex>,
    draws: Vec<Draw>,
    screen_points: [f32; 2],
    /// User texture ids drawn by the current UI.
    pub user_ids: Vec<u64>,
}

fn sampler(device: &ID3D11Device, filter: D3D11_FILTER) -> windows::core::Result<ID3D11SamplerState> {
    let sd = D3D11_SAMPLER_DESC {
        Filter: filter,
        AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
        AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
        MaxLOD: f32::MAX,
        ComparisonFunc: D3D11_COMPARISON_NEVER,
        ..Default::default()
    };
    let mut s = None;
    // SAFETY: valid descriptor and out pointer.
    unsafe { device.CreateSamplerState(&sd, Some(&mut s))? };
    s.ok_or_else(windows::core::Error::empty)
}

impl EguiRenderer {
    pub fn new(device: &ID3D11Device) -> Result<Self, String> {
        let e = |e: windows::core::Error| e.to_string();
        let vs_code = compile(SHADER, s!("vs"), s!("vs_4_0"))?;
        let ps_code = compile(SHADER, s!("ps"), s!("ps_4_0"))?;
        // SAFETY: COM calls with valid descriptors and out pointers.
        unsafe {
            let mut vs = None;
            device.CreateVertexShader(&vs_code, None, Some(&mut vs)).map_err(e)?;
            let mut ps = None;
            device.CreatePixelShader(&ps_code, None, Some(&mut ps)).map_err(e)?;
            let elements = [
                D3D11_INPUT_ELEMENT_DESC {
                    SemanticName: s!("POSITION"),
                    SemanticIndex: 0,
                    Format: DXGI_FORMAT_R32G32_FLOAT,
                    InputSlot: 0,
                    AlignedByteOffset: 0,
                    InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                    InstanceDataStepRate: 0,
                },
                D3D11_INPUT_ELEMENT_DESC {
                    SemanticName: s!("TEXCOORD"),
                    SemanticIndex: 0,
                    Format: DXGI_FORMAT_R32G32_FLOAT,
                    InputSlot: 0,
                    AlignedByteOffset: 8,
                    InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                    InstanceDataStepRate: 0,
                },
                D3D11_INPUT_ELEMENT_DESC {
                    SemanticName: s!("COLOR"),
                    SemanticIndex: 0,
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    InputSlot: 0,
                    AlignedByteOffset: 16,
                    InputSlotClass: D3D11_INPUT_PER_VERTEX_DATA,
                    InstanceDataStepRate: 0,
                },
            ];
            let mut layout = None;
            device.CreateInputLayout(&elements, &vs_code, Some(&mut layout)).map_err(e)?;
            let mut bd = D3D11_BLEND_DESC::default();
            bd.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_ONE,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_INV_DEST_ALPHA,
                DestBlendAlpha: D3D11_BLEND_ONE,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut blend = None;
            device.CreateBlendState(&bd, Some(&mut blend)).map_err(e)?;
            let rd = D3D11_RASTERIZER_DESC {
                FillMode: D3D11_FILL_SOLID,
                CullMode: D3D11_CULL_NONE,
                ScissorEnable: true.into(),
                DepthClipEnable: false.into(),
                ..Default::default()
            };
            let mut raster = None;
            device.CreateRasterizerState(&rd, Some(&mut raster)).map_err(e)?;
            let cbd = D3D11_BUFFER_DESC {
                ByteWidth: 16,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            };
            let mut constants = None;
            device.CreateBuffer(&cbd, None, Some(&mut constants)).map_err(e)?;
            Ok(Self {
                vs: vs.ok_or("no vs")?,
                ps: ps.ok_or("no ps")?,
                layout: layout.ok_or("no layout")?,
                linear: sampler(device, D3D11_FILTER_MIN_MAG_MIP_LINEAR).map_err(e)?,
                nearest: sampler(device, D3D11_FILTER_MIN_MAG_MIP_POINT).map_err(e)?,
                blend: blend.ok_or("no blend")?,
                raster: raster.ok_or("no raster")?,
                constants: constants.ok_or("no cb")?,
                vb: None,
                ib: None,
                textures: HashMap::new(),
                draws: Vec::new(),
                screen_points: [1.0, 1.0],
                user_ids: Vec::new(),
            })
        }
    }

    /// Applies new or changed managed textures (before drawing).
    pub fn set_textures(&mut self, device: &ID3D11Device, ctx: &ID3D11DeviceContext, delta: &egui::TexturesDelta) {
        for (id, deltas) in &delta.set {
            for d in deltas {
                let egui::ImageData::Color(img) = &d.image;
                let nearest = d.options.magnification == egui::TextureFilter::Nearest;
                let [w, h] = img.size;
                if w == 0 || h == 0 {
                    continue;
                }
                let bytes: &[u8] = unsafe { std::slice::from_raw_parts(img.pixels.as_ptr().cast::<u8>(), img.pixels.len() * 4) };
                match d.pos {
                    None => {
                        let reuse = self.textures.get(id).is_some_and(|t| t.size == img.size);
                        if !reuse {
                            match create_texture(device, w as u32, h as u32) {
                                Ok((texture, view)) => {
                                    self.textures.insert(*id, Tex { texture, view, size: img.size, nearest });
                                }
                                Err(e) => {
                                    log::error!("egui texture {w}x{h}: {e}");
                                    continue;
                                }
                            }
                        }
                        if let Some(t) = self.textures.get_mut(id) {
                            t.nearest = nearest;
                            // SAFETY: the texture is w x h RGBA8; `bytes` has that size.
                            unsafe { ctx.UpdateSubresource(&t.texture, 0, None, bytes.as_ptr().cast(), (w * 4) as u32, 0) };
                        }
                    }
                    Some([x, y]) => {
                        if let Some(t) = self.textures.get(id) {
                            if x + w > t.size[0] || y + h > t.size[1] {
                                continue;
                            }
                            let b = D3D11_BOX { left: x as u32, top: y as u32, front: 0, right: (x + w) as u32, bottom: (y + h) as u32, back: 1 };
                            // SAFETY: the box lies inside the texture; data is w x h RGBA8.
                            unsafe { ctx.UpdateSubresource(&t.texture, 0, Some(&b), bytes.as_ptr().cast(), (w * 4) as u32, 0) };
                        }
                    }
                }
            }
        }
    }

    pub fn free_textures(&mut self, delta: &egui::TexturesDelta) {
        for id in &delta.free {
            self.textures.remove(id);
        }
    }

    /// Uploads a new UI's geometry. Called only when the UI changed.
    pub fn upload(
        &mut self,
        device: &ID3D11Device,
        ctx: &ID3D11DeviceContext,
        primitives: &[ClippedPrimitive],
        ppp: f32,
        screen_px: (u32, u32),
    ) -> Result<(), String> {
        self.draws.clear();
        self.user_ids.clear();
        self.screen_points = [screen_px.0 as f32 / ppp, screen_px.1 as f32 / ppp];
        let (mut nv, mut ni) = (0usize, 0usize);
        for p in primitives {
            if let Primitive::Mesh(m) = &p.primitive {
                nv += m.vertices.len();
                ni += m.indices.len();
            }
        }
        if nv == 0 || ni == 0 {
            return Ok(());
        }
        let vb = ensure_buffer(device, &mut self.vb, nv * std::mem::size_of::<egui::epaint::Vertex>(), D3D11_BIND_VERTEX_BUFFER)?;
        let ib = ensure_buffer(device, &mut self.ib, ni * 4, D3D11_BIND_INDEX_BUFFER)?;
        let e = |e: windows::core::Error| e.to_string();
        // SAFETY: both buffers are dynamic, mapped with WRITE_DISCARD, and at
        // least as large as the data copied into them.
        unsafe {
            let mut vm = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&vb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut vm)).map_err(e)?;
            let mut im = D3D11_MAPPED_SUBRESOURCE::default();
            if let Err(err) = ctx.Map(&ib, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut im)) {
                ctx.Unmap(&vb, 0);
                return Err(err.to_string());
            }
            let (mut vo, mut io) = (0usize, 0usize);
            let (w, h) = (screen_px.0 as f32, screen_px.1 as f32);
            for p in primitives {
                let Primitive::Mesh(m) = &p.primitive else { continue };
                if m.indices.is_empty() {
                    continue;
                }
                std::ptr::copy_nonoverlapping(m.vertices.as_ptr(), (vm.pData as *mut egui::epaint::Vertex).add(vo), m.vertices.len());
                std::ptr::copy_nonoverlapping(m.indices.as_ptr(), (im.pData as *mut u32).add(io), m.indices.len());
                let c = p.clip_rect;
                let scissor = RECT {
                    left: (c.min.x * ppp).round().clamp(0.0, w) as i32,
                    top: (c.min.y * ppp).round().clamp(0.0, h) as i32,
                    right: (c.max.x * ppp).round().clamp(0.0, w) as i32,
                    bottom: (c.max.y * ppp).round().clamp(0.0, h) as i32,
                };
                if scissor.right > scissor.left && scissor.bottom > scissor.top {
                    if let TextureId::User(u) = m.texture_id {
                        if !self.user_ids.contains(&u) {
                            self.user_ids.push(u);
                        }
                    }
                    self.draws.push(Draw {
                        texture: m.texture_id,
                        scissor,
                        index_start: io as u32,
                        index_count: m.indices.len() as u32,
                        base_vertex: vo as i32,
                    });
                }
                vo += m.vertices.len();
                io += m.indices.len();
            }
            ctx.Unmap(&ib, 0);
            ctx.Unmap(&vb, 0);
        }
        Ok(())
    }

    /// Draws the uploaded UI. `user` resolves user textures (streams,
    /// animations); a missing one is skipped.
    pub fn draw(
        &self,
        ctx: &ID3D11DeviceContext,
        rtv: &ID3D11RenderTargetView,
        screen_px: (u32, u32),
        user: &dyn Fn(u64) -> Option<ID3D11ShaderResourceView>,
    ) -> Result<(), String> {
        let (Some((vb, _)), Some((ib, _))) = (&self.vb, &self.ib) else { return Ok(()) };
        if self.draws.is_empty() {
            return Ok(());
        }
        // SAFETY: COM calls on this thread's context with live resources.
        unsafe {
            let mut m = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&self.constants, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut m)).map_err(|e| e.to_string())?;
            std::ptr::copy_nonoverlapping([self.screen_points[0], self.screen_points[1], 0.0, 0.0].as_ptr(), m.pData.cast::<f32>(), 4);
            ctx.Unmap(&self.constants, 0);

            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: screen_px.0 as f32,
                Height: screen_px.1 as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            ctx.RSSetState(&self.raster);
            ctx.OMSetBlendState(&self.blend, None, 0xffff_ffff);
            ctx.OMSetDepthStencilState(None, 0);
            ctx.IASetInputLayout(&self.layout);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            let stride = std::mem::size_of::<egui::epaint::Vertex>() as u32;
            let offset = 0u32;
            ctx.IASetVertexBuffers(0, 1, Some(&Some(vb.clone())), Some(&stride), Some(&offset));
            ctx.IASetIndexBuffer(ib, DXGI_FORMAT_R32_UINT, 0);
            ctx.VSSetShader(&self.vs, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
            ctx.PSSetShader(&self.ps, None);
            let mut bound_sampler_nearest: Option<bool> = None;
            for d in &self.draws {
                let (view, nearest) = match d.texture {
                    TextureId::Managed(_) => match self.textures.get(&d.texture) {
                        Some(t) => (Some(t.view.clone()), t.nearest),
                        None => (None, false),
                    },
                    TextureId::User(u) => (user(u), false),
                };
                let Some(view) = view else { continue };
                if bound_sampler_nearest != Some(nearest) {
                    let s = if nearest { &self.nearest } else { &self.linear };
                    ctx.PSSetSamplers(0, Some(&[Some(s.clone())]));
                    bound_sampler_nearest = Some(nearest);
                }
                ctx.PSSetShaderResources(0, Some(&[Some(view)]));
                ctx.RSSetScissorRects(Some(&[d.scissor]));
                ctx.DrawIndexed(d.index_count, d.index_start, d.base_vertex);
            }
            ctx.PSSetShaderResources(0, Some(&[None]));
        }
        Ok(())
    }
}

fn ensure_buffer(
    device: &ID3D11Device,
    slot: &mut Option<(ID3D11Buffer, usize)>,
    bytes: usize,
    bind: D3D11_BIND_FLAG,
) -> Result<ID3D11Buffer, String> {
    if let Some((b, cap)) = slot {
        if *cap >= bytes {
            return Ok(b.clone());
        }
    }
    // Grow geometrically so a growing chat doesn't reallocate every frame.
    let cap = bytes.next_power_of_two().max(64 * 1024);
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: cap as u32,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: bind.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        ..Default::default()
    };
    let mut b = None;
    // SAFETY: valid descriptor and out pointer.
    unsafe { device.CreateBuffer(&desc, None, Some(&mut b)) }.map_err(|e| e.to_string())?;
    let b = b.ok_or("no buffer")?;
    *slot = Some((b.clone(), cap));
    Ok(b)
}

/// An RGBA8 shader-resource texture.
pub(crate) fn create_texture(device: &ID3D11Device, w: u32, h: u32) -> windows::core::Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let (mut t, mut v) = (None, None);
    // SAFETY: valid descriptor and out pointers.
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut t))?;
        let t = t.ok_or_else(windows::core::Error::empty)?;
        device.CreateShaderResourceView(&t, None, Some(&mut v))?;
        Ok((t, v.ok_or_else(windows::core::Error::empty)?))
    }
}
