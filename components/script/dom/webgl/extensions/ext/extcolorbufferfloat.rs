/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_canvas_traits::webgl::WebGLVersion;

use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;

#[dom_struct]
pub(crate) struct EXTColorBufferFloat {
    reflector_: Reflector,
}

impl EXTColorBufferFloat {
    fn new_inherited() -> EXTColorBufferFloat {
        Self {
            reflector_: Reflector::new(),
        }
    }
}

impl WebGLExtension for EXTColorBufferFloat {
    type Extension = EXTColorBufferFloat;
    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<EXTColorBufferFloat> {
        reflect_dom_object_with_cx(Box::new(Self::new_inherited()), &*ctx.global(), cx)
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::Specific(WebGLVersion::WebGL2)
    }

    fn is_supported(ext: &WebGLExtensions) -> bool {
        // ANGLE が広告する拡張名で判定する。
        // 対象名は実機ログ（Meta XR Simulator 経由の ANGLE）で確認したものに限定する。
        ext.supports_gl_extension("GL_EXT_color_buffer_float")
    }

    // 有効化時の登録は不要である。浮動小数点形式の扱いは
    // WebGLExtensions::is_ext_color_buffer_float_enabled の判定先
    //（レンダーバッファの internal_format 許可）で切り替える。
    fn enable(_ext: &WebGLExtensions) {}

    fn name() -> &'static str {
        "EXT_color_buffer_float"
    }
}
