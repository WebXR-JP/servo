//! Windows(D3D11)向けのXR提示ループ本体。

use std::fs::{File, OpenOptions};
use std::mem;
use std::ptr;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use openxr::d3d::{D3D11, SessionCreateInfoD3D11};
use openxr::{
    self, ApplicationInfo, CompositionLayerBase, CompositionLayerQuad, Event, ExtensionSet,
    Extent2Df, Extent2Di, FormFactor, FrameState, Graphics, Offset2Di, Posef, Quaternionf, Rect2Di,
    ReferenceSpaceType, SessionState, Swapchain, SwapchainCreateFlags, SwapchainCreateInfo,
    SwapchainUsageFlags, Vector3f, Version, ViewConfigurationType,
};
use winapi::Interface;
use winapi::shared::dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1};
use winapi::shared::dxgiformat::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_R8G8B8A8_UNORM_SRGB,
};
use winapi::shared::winerror::{DXGI_ERROR_NOT_FOUND, S_OK};
use winapi::um::d3d11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Resource,
};
use winapi::um::d3dcommon::D3D_DRIVER_TYPE_UNKNOWN;
use wio::com::ComPtr;

/// 提示画像の固定サイズ。stereoは不要(Quad/BOTH)のため正方形1枚でよい。
const IMAGE_SIZE: u32 = 1024;
/// カラーバーの本数。
const BAR_COUNT: u32 = 8;
/// 四隅マーカーの一辺。
const MARKER: u32 = 64;
/// 動く白四角の一辺。
const SQUARE: u32 = 128;

struct Logger {
    file: Mutex<File>,
    pid: u32,
    start: Instant,
}

impl Logger {
    fn log(&self, value: serde_json::Value) {
        let mut line =
            serde_json::to_string(&value).unwrap_or_else(|_| "{\"event\":\"encode_error\"}".into());
        line.push('\n');
        if let Ok(mut file) = self.file.lock() {
            use std::io::Write as _;
            let _ = file.write_all(line.as_bytes());
            let _ = file.flush();
        }
    }

    fn frame_row(
        &self,
        frame: u64,
        state: &str,
        begun: bool,
        wait_ok: bool,
        begin_ok: bool,
        gen_ms: Option<f64>,
        upload_ms: Option<f64>,
        should_render: bool,
        submitted: bool,
        end_ok: bool,
        submits_total: u64,
        format: u32,
        error: Option<&str>,
    ) {
        self.log(serde_json::json!({
            "event": "frame",
            "pid": self.pid,
            "tid": format!("{:?}", std::thread::current().id()),
            "elapsed_ms": self.start.elapsed().as_secs_f64() * 1000.0,
            "frame": frame,
            "session_state": state,
            "session_begun": begun,
            "wait_ok": wait_ok,
            "begin_ok": begin_ok,
            "gen_ms": gen_ms,
            "upload_ms": upload_ms,
            "should_render": should_render,
            "submitted": submitted,
            "end_ok": end_ok,
            "submits_total": submits_total,
            "swapchain_format": format,
            "error": error,
        }));
    }
}

fn load_logger() -> Logger {
    let path = std::env::var("XR_PRESENT_LOG").unwrap_or_else(|_| "xr-present.jsonl".into());
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        // 想定条件: ログを開けない場合はPoCとして続行不能なため初期化失敗として落とす。
        .unwrap_or_else(|e| {
            eprintln!("error: cannot open XR present log {path}: {e}");
            std::process::exit(2);
        });
    Logger {
        file: Mutex::new(file),
        pid: std::process::id(),
        start: Instant::now(),
    }
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// `xrGetD3D11GraphicsRequirementsKHR`のLUIDに一致するアダプタでデバイスを作る。
fn create_d3d11_device(
    adapter_luid_low: u32,
    adapter_luid_high: i32,
    min_feature_level: i32,
) -> Result<(ComPtr<ID3D11Device>, ComPtr<ID3D11DeviceContext>), String> {
    unsafe {
        let mut factory_ptr: *mut IDXGIFactory1 = ptr::null_mut();
        let hr = CreateDXGIFactory1(
            &IDXGIFactory1::uuidof(),
            &mut factory_ptr as *mut _ as *mut _,
        );
        if hr != S_OK {
            return Err(format!("CreateDXGIFactory1 failed: {hr:#x}"));
        }
        let factory = ComPtr::from_raw(factory_ptr);

        let mut index = 0;
        let adapter = loop {
            let mut adapter_ptr: *mut IDXGIAdapter1 = ptr::null_mut();
            let hr = factory.EnumAdapters1(index, &mut adapter_ptr);
            if hr == DXGI_ERROR_NOT_FOUND {
                return Err("no DXGI adapter matches runtime LUID".into());
            }
            if hr != S_OK {
                return Err(format!("EnumAdapters1({index}) failed: {hr:#x}"));
            }
            // 想定条件: EnumAdapters1がS_OKを返した場合は有効なポインタを返す。
            let candidate = ComPtr::from_raw(adapter_ptr);
            let mut desc = mem::zeroed();
            let hr = candidate.GetDesc1(&mut desc);
            if hr == S_OK
                && desc.AdapterLuid.LowPart == adapter_luid_low
                && desc.AdapterLuid.HighPart == adapter_luid_high
            {
                break candidate;
            }
            index += 1;
        };

        let mut device: *mut ID3D11Device = ptr::null_mut();
        let mut context: *mut ID3D11DeviceContext = ptr::null_mut();
        let mut feature_level = 0;
        // 機能レベル配列を渡さず既定の降順選択に任せ、戻り値が最小要件を満たすか検証する。
        let hr = D3D11CreateDevice(
            adapter.as_raw() as *mut IDXGIAdapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            ptr::null_mut(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            ptr::null(),
            0,
            D3D11_SDK_VERSION,
            &mut device,
            &mut feature_level,
            &mut context,
        );
        if hr != S_OK {
            return Err(format!("D3D11CreateDevice failed: {hr:#x}"));
        }
        if device.is_null() || context.is_null() {
            return Err("D3D11CreateDevice returned null device/context".into());
        }
        if (feature_level as i32) < min_feature_level {
            return Err(format!(
                "device feature level {feature_level:#x} below runtime minimum {min_feature_level:#x}"
            ));
        }
        Ok((ComPtr::from_raw(device), ComPtr::from_raw(context)))
    }
}

/// 8本のカラーバー + 動く白四角 + 四隅マーカーを生成する。
/// `bgra_order`が真の場合はB,G,R,A順、偽の場合はR,G,B,A順で格納する。
fn generate_pattern(frame: u64, bgra_order: bool) -> Vec<u8> {
    const BARS: [(u8, u8, u8); 8] = [
        (255, 0, 0),
        (0, 255, 0),
        (0, 0, 255),
        (255, 255, 0),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
        (32, 32, 32),
    ];
    let size = IMAGE_SIZE as usize;
    let mut pixels = vec![0u8; size * size * 4];
    let bar_w = size / BAR_COUNT as usize;
    for y in 0..size {
        for x in 0..size {
            let (r, g, b) = BARS[(x / bar_w).min(BARS.len() - 1)];
            let i = (y * size + x) * 4;
            if bgra_order {
                pixels[i] = b;
                pixels[i + 1] = g;
                pixels[i + 2] = r;
                pixels[i + 3] = 255;
            } else {
                pixels[i] = r;
                pixels[i + 1] = g;
                pixels[i + 2] = b;
                pixels[i + 3] = 255;
            }
        }
    }
    // 動く白四角(黒縁付き)。
    let sq = SQUARE as usize;
    let sx = ((frame * 8) % (size - sq) as u64) as usize;
    let sy = (size - sq) / 2;
    let put = |pixels: &mut [u8], x: usize, y: usize, r: u8, g: u8, b: u8| {
        let i = (y * size + x) * 4;
        if bgra_order {
            pixels[i] = b;
            pixels[i + 1] = g;
            pixels[i + 2] = r;
        } else {
            pixels[i] = r;
            pixels[i + 1] = g;
            pixels[i + 2] = b;
        }
        pixels[i + 3] = 255;
    };
    for y in sy..sy + sq {
        for x in sx..sx + sq {
            let border = x - sx < 4 || sx + sq - x <= 4 || y - sy < 4 || sy + sq - y <= 4;
            if border {
                put(&mut pixels, x, y, 0, 0, 0);
            } else {
                put(&mut pixels, x, y, 255, 255, 255);
            }
        }
    }
    // 四隅マーカー(白地に黒のくり抜き)。
    let m = MARKER as usize;
    for &(ox, oy) in &[(0, 0), (size - m, 0), (0, size - m), (size - m, size - m)] {
        for y in oy..oy + m {
            for x in ox..ox + m {
                let inner = x - ox >= 16 && ox + m - x > 16 && y - oy >= 16 && oy + m - y > 16;
                if inner {
                    put(&mut pixels, x, y, 0, 0, 0);
                } else {
                    put(&mut pixels, x, y, 255, 255, 255);
                }
            }
        }
    }
    pixels
}

fn pick_format(formats: &[u32]) -> Result<(u32, bool), String> {
    // BGRA8優先。無ければRGBA8系を選びCPU側でswizzleする。
    const PREFERENCE: [(u32, bool); 4] = [
        (DXGI_FORMAT_B8G8R8A8_UNORM_SRGB, true),
        (DXGI_FORMAT_B8G8R8A8_UNORM, true),
        (DXGI_FORMAT_R8G8B8A8_UNORM_SRGB, false),
        (DXGI_FORMAT_R8G8B8A8_UNORM, false),
    ];
    for (candidate, bgra) in PREFERENCE {
        if formats.contains(&candidate) {
            return Ok((candidate, bgra));
        }
    }
    Err(format!("no supported swapchain format in {formats:?}"))
}

fn state_name(state: SessionState) -> &'static str {
    match state {
        SessionState::UNKNOWN => "UNKNOWN",
        SessionState::IDLE => "IDLE",
        SessionState::READY => "READY",
        SessionState::SYNCHRONIZED => "SYNCHRONIZED",
        SessionState::VISIBLE => "VISIBLE",
        SessionState::FOCUSED => "FOCUSED",
        SessionState::STOPPING => "STOPPING",
        SessionState::LOSS_PENDING => "LOSS_PENDING",
        SessionState::EXITING => "EXITING",
        _ => "OTHER",
    }
}

/// 初期化失敗はプロセス終了コード2で落とす(フレームループ外のため`expect`可)。
pub(crate) fn run(seconds: Option<u64>) {
    let logger = load_logger();
    logger.log(serde_json::json!({
        "event": "init",
        "pid": logger.pid,
        "unix_ms": unix_ms(),
        "seconds": seconds,
    }));

    let entry = unsafe { openxr::Entry::load().expect("Entry::load (openxr_loader.dll)") };
    let mut exts = ExtensionSet::default();
    exts.khr_d3d11_enable = true;
    let app_info = ApplicationInfo {
        application_name: "servoshell-xr-present",
        application_version: 0,
        engine_name: "servo-xr-present-poc",
        engine_version: 0,
        api_version: Version::new(1, 0, 36),
    };
    let instance = entry
        .create_instance(&app_info, &exts, &[])
        .expect("xrCreateInstance");
    let system = instance
        .system(FormFactor::HEAD_MOUNTED_DISPLAY)
        .expect("instance.system(HMD)");
    let requirements =
        D3D11::requirements(&instance, system).expect("getD3D11GraphicsRequirementsKHR");
    logger.log(serde_json::json!({
        "event": "requirements",
        "pid": logger.pid,
        "luid_low": requirements.adapter_luid.LowPart,
        "luid_high": requirements.adapter_luid.HighPart,
        "min_feature_level": requirements.min_feature_level,
    }));

    let (device, context) = match create_d3d11_device(
        requirements.adapter_luid.LowPart,
        requirements.adapter_luid.HighPart,
        requirements.min_feature_level,
    ) {
        Ok(pair) => pair,
        Err(e) => {
            logger.log(serde_json::json!({"event": "init_error", "error": e}));
            eprintln!("error: {e}");
            std::process::exit(2);
        },
    };
    // セッションがデバイスを参照する間はComPtrを保持する。`run`終了時に解放される。
    let device_ptr = device.as_raw();
    let context_ptr = context.as_raw();
    let _device_keepalive = (device, context);

    let (session, mut frame_waiter, mut frame_stream) = unsafe {
        instance
            .create_session::<D3D11>(
                system,
                &SessionCreateInfoD3D11 {
                    device: device_ptr as *mut _,
                },
            )
            .expect("xrCreateSession(D3D11)")
    };
    // Session<->&mutの排他利用を静的に保証するため以降`session`は共有参照のみで使う。
    let session = session;

    let formats = session
        .enumerate_swapchain_formats()
        .expect("enumerateSwapchainFormats");
    let (format, bgra_order) = match pick_format(&formats) {
        Ok(pair) => pair,
        Err(e) => {
            logger.log(serde_json::json!({"event": "init_error", "formats": formats, "error": e}));
            eprintln!("error: {e}");
            std::process::exit(2);
        },
    };
    logger.log(serde_json::json!({
        "event": "swapchain_format",
        "pid": logger.pid,
        "formats": formats,
        "chosen": format,
        "bgra_order": bgra_order,
    }));

    let swapchain_info = SwapchainCreateInfo {
        create_flags: SwapchainCreateFlags::EMPTY,
        usage_flags: SwapchainUsageFlags::COLOR_ATTACHMENT | SwapchainUsageFlags::SAMPLED,
        width: IMAGE_SIZE,
        height: IMAGE_SIZE,
        format,
        sample_count: 1,
        face_count: 1,
        array_size: 1,
        mip_count: 1,
    };
    let mut swapchain = session
        .create_swapchain(&swapchain_info)
        .expect("xrCreateSwapchain");
    let images = swapchain
        .enumerate_images()
        .expect("enumerateSwapchainImages");
    if images.is_empty() {
        logger.log(serde_json::json!({"event": "init_error", "error": "swapchain has no images"}));
        eprintln!("error: swapchain has no images");
        std::process::exit(2);
    }

    let identity = Posef {
        orientation: Quaternionf {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 1.0,
        },
        position: Vector3f {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        },
    };
    let space = session
        .create_reference_space(ReferenceSpaceType::LOCAL, identity)
        .expect("createReferenceSpace(LOCAL)");
    let blend = instance
        .enumerate_environment_blend_modes(system, ViewConfigurationType::PRIMARY_STEREO)
        .expect("enumerateEnvironmentBlendModes")[0];

    let quad_pose = Posef {
        orientation: Quaternionf {
            x: 0.0,
            y: 0.0,
            z: 0.0,
            w: 1.0,
        },
        position: Vector3f {
            x: 0.0,
            y: 0.0,
            z: -2.0,
        },
    };
    let sub_rect = Rect2Di {
        offset: Offset2Di { x: 0, y: 0 },
        extent: Extent2Di {
            width: IMAGE_SIZE as i32,
            height: IMAGE_SIZE as i32,
        },
    };

    let deadline = seconds
        .filter(|s| *s > 0)
        .map(|s| Instant::now() + Duration::from_secs(s));
    let mut state: SessionState = SessionState::IDLE;
    let mut begun = false;
    let mut frame: u64 = 0;
    let mut submits: u64 = 0;
    let mut focused_at_ms: Option<u128> = None;
    let mut buffer = openxr::EventDataBuffer::new();

    loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        // セッション状態の更新。STOPPINGでは`xrEndSession`、LOSS/EXITINGでは終了する。
        let mut exit_requested = false;
        loop {
            match instance.poll_event(&mut buffer) {
                Ok(Some(Event::SessionStateChanged(changed))) => {
                    state = changed.state();
                    logger.log(serde_json::json!({
                        "event": "state",
                        "pid": logger.pid,
                        "elapsed_ms": logger.start.elapsed().as_secs_f64() * 1000.0,
                        "session_state": state_name(state),
                    }));
                    match state {
                        SessionState::READY if !begun => {
                            match session.begin(ViewConfigurationType::PRIMARY_STEREO) {
                                Ok(_) => begun = true,
                                Err(e) => logger.log(serde_json::json!({
                                    "event": "begin_error", "error": format!("{e:?}"),
                                })),
                            }
                        },
                        SessionState::STOPPING if begun => {
                            if let Err(e) = session.end() {
                                logger.log(serde_json::json!({
                                    "event": "end_error", "error": format!("{e:?}"),
                                }));
                            }
                            begun = false;
                        },
                        SessionState::EXITING | SessionState::LOSS_PENDING => {
                            exit_requested = true;
                        },
                        SessionState::FOCUSED if focused_at_ms.is_none() => {
                            focused_at_ms = Some(unix_ms());
                        },
                        _ => {},
                    }
                },
                Ok(Some(Event::InstanceLossPending(_))) => exit_requested = true,
                Ok(Some(_)) => {},
                Ok(None) => break,
                Err(e) => {
                    logger.log(serde_json::json!({
                        "event": "poll_error", "error": format!("{e:?}"),
                    }));
                    break;
                },
            }
        }
        if exit_requested {
            break;
        }
        if !begun {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }

        // 規定の順序: Wait -> Begin -> Acquire -> WaitImage -> Upload -> Release -> End。
        let waited: Result<FrameState, String> = frame_waiter.wait().map_err(|e| format!("{e:?}"));
        let waited = match waited {
            Ok(s) => s,
            Err(e) => {
                logger.frame_row(
                    frame,
                    state_name(state),
                    begun,
                    false,
                    false,
                    None,
                    None,
                    false,
                    false,
                    false,
                    submits,
                    format,
                    Some(&e),
                );
                std::thread::sleep(Duration::from_millis(5));
                continue;
            },
        };
        let begin_result = frame_stream.begin().map_err(|e| format!("{e:?}"));
        let begin_ok = begin_result.is_ok();
        if let Err(e) = begin_result {
            logger.frame_row(
                frame,
                state_name(state),
                begun,
                true,
                false,
                None,
                None,
                waited.should_render,
                false,
                false,
                submits,
                format,
                Some(&e),
            );
            continue;
        }

        let mut upload_ms = None;
        let mut gen_ms = None;
        let mut submitted = false;
        let mut end_ok = false;
        let mut error: Option<String> = None;
        if waited.should_render {
            match upload_frame(
                &mut swapchain,
                context_ptr,
                &images,
                frame,
                bgra_order,
                &mut gen_ms,
                &mut upload_ms,
            ) {
                Ok(()) => {
                    let quad = CompositionLayerQuad::new()
                        .space(&space)
                        .eye_visibility(openxr::EyeVisibility::BOTH)
                        .sub_image(
                            openxr::SwapchainSubImage::new()
                                .swapchain(&swapchain)
                                .image_array_index(0)
                                .image_rect(sub_rect),
                        )
                        .pose(quad_pose)
                        .size(Extent2Df {
                            width: 1.0,
                            height: 1.0,
                        });
                    let base: &CompositionLayerBase<D3D11> = &quad;
                    match frame_stream.end(waited.predicted_display_time, blend, &[base]) {
                        Ok(()) => {
                            submitted = true;
                            end_ok = true;
                            submits += 1;
                        },
                        Err(e) => error = Some(format!("endFrame: {e:?}")),
                    }
                },
                Err(e) => {
                    // アップロード経路で失敗した場合は当該フレームの提示を諦め、
                    // 空レイヤーでEndFrameしてセッションを維持する。
                    error = Some(e);
                    if frame_stream
                        .end(waited.predicted_display_time, blend, &[])
                        .is_ok()
                    {
                        end_ok = true;
                    }
                },
            }
        } else {
            // shouldRender=falseのフレームはレイヤー0枚でEndFrameする。
            end_ok = frame_stream
                .end(waited.predicted_display_time, blend, &[])
                .is_ok();
            if !end_ok {
                error = Some("endFrame(empty) failed".into());
            }
        }
        logger.frame_row(
            frame,
            state_name(state),
            begun,
            true,
            begin_ok,
            gen_ms,
            upload_ms,
            waited.should_render,
            submitted,
            end_ok,
            submits,
            format,
            error.as_deref(),
        );
        frame += 1;
    }

    if begun {
        let _ = session.end();
    }
    // 子ハンドルから順に破棄してランタイムの終了処理を正常に通す。
    // `process::exit`ではデストラクタが走らないため、ここで明示する。
    drop(space);
    drop(swapchain);
    drop(frame_stream);
    drop(frame_waiter);
    drop(session);
    drop(instance);
    drop(_device_keepalive);
    let exit_code = if submits > 0 { 0 } else { 3 };
    logger.log(serde_json::json!({
        "event": "summary",
        "pid": logger.pid,
        "unix_ms": unix_ms(),
        "frames": frame,
        "submits": submits,
        "focused_at_ms": focused_at_ms,
        "exit_code": exit_code,
    }));
    log::info!("xr-present finished: frames={frame} submits={submits} exit={exit_code}");
    std::process::exit(exit_code);
}

/// Acquire -> WaitImage -> CPUアップロード -> Release の一連処理。
/// 失敗時は`Err`文字列を返し、呼び出し側がログに残す(握り潰さない)。
fn upload_frame(
    swapchain: &mut Swapchain<D3D11>,
    context_ptr: *mut ID3D11DeviceContext,
    images: &[<D3D11 as Graphics>::SwapchainImage],
    frame: u64,
    bgra_order: bool,
    gen_ms: &mut Option<f64>,
    upload_ms: &mut Option<f64>,
) -> Result<(), String> {
    let index = swapchain
        .acquire_image()
        .map_err(|e| format!("acquire: {e:?}"))? as usize;
    // 以降の失敗では獲得済みイメージを返却し、スワップチェーンの枯渇を避ける。
    if let Err(e) = swapchain.wait_image(openxr::Duration::INFINITE) {
        let _ = swapchain.release_image();
        return Err(format!("waitImage: {e:?}"));
    }
    let texture = match images.get(index) {
        Some(texture) => *texture,
        None => {
            let _ = swapchain.release_image();
            return Err(format!("image index {index} out of range"));
        },
    };
    let gen_start = Instant::now();
    let pixels = generate_pattern(frame, bgra_order);
    let gen_elapsed_ms = gen_start.elapsed().as_secs_f64() * 1000.0;
    let upload_start = Instant::now();
    (unsafe {
        // SAFETY: textureはenumerate_images由来の有効なID3D11Texture2Dであり、
        // WaitImage済みのためCPU書き込み可能。contextはD3D11CreateDevice由来で有効。
        // UpdateSubresourceは同期コピーでありpixelsの寿命内に戻る。
        (*context_ptr).UpdateSubresource(
            texture as *mut ID3D11Resource,
            0,
            ptr::null(),
            pixels.as_ptr() as *const _,
            IMAGE_SIZE * 4,
            0,
        );
        (*context_ptr).Flush();
    });
    *gen_ms = Some(gen_elapsed_ms);
    *upload_ms = Some(upload_start.elapsed().as_secs_f64() * 1000.0);
    swapchain
        .release_image()
        .map_err(|e| format!("release: {e:?}"))?;
    Ok(())
}
