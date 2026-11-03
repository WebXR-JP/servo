/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_cx};
use servo_canvas_traits::webgl::{TexFormat, WebGLVersion};

use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;
use crate::dom::webgl::webgltexture::{TexCompression, TexCompressionValidation};

#[dom_struct]
pub(crate) struct WEBGLCompressedTextureS3TCSrgb {
    reflector_: Reflector,
}

impl WEBGLCompressedTextureS3TCSrgb {
    fn new_inherited() -> WEBGLCompressedTextureS3TCSrgb {
        Self {
            reflector_: Reflector::new(),
        }
    }
}

impl WebGLExtension for WEBGLCompressedTextureS3TCSrgb {
    type Extension = WEBGLCompressedTextureS3TCSrgb;
    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<WEBGLCompressedTextureS3TCSrgb> {
        reflect_dom_object_with_cx(Box::new(Self::new_inherited()), &*ctx.global(), cx)
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::Specific(WebGLVersion::WebGL2)
    }

    fn is_supported(ext: &WebGLExtensions) -> bool {
        // 拡張文字列がある場合は従来どおり公開する。
        // macOS のデスクトップ GL (GL 4.1) は拡張文字列に
        // `GL_EXT_texture_compression_s3tc_srgb` を出さないが、
        // sRGB S3TC の内部フォーマット (0x8C4C/0x8C4D/0x8C4F) を受け付ける。
        // その環境では WebGL2 かつデスクトップ GL かつ S3TC 対応を条件に公開する。
        // WebGL2 条件は `get_or_init_extension()` が `spec()` のバージョンを
        // 見ないため、WebGL1 に露出させない目的で付ける。
        ext.supports_gl_extension("GL_EXT_texture_compression_s3tc_srgb") ||
            (ext.webgl_version() == WebGLVersion::WebGL2 &&
                !ext.is_gles() &&
                ext.supports_gl_extension("GL_EXT_texture_compression_s3tc"))
    }

    fn enable(ext: &WebGLExtensions) {
        ext.add_tex_compression_formats(&[
            TexCompression {
                format: TexFormat::CompressedSrgbS3tcDxt1,
                bytes_per_block: 8,
                block_width: 4,
                block_height: 4,
                validation: TexCompressionValidation::S3TC,
            },
            TexCompression {
                format: TexFormat::CompressedSrgbAlphaS3tcDxt1,
                bytes_per_block: 8,
                block_width: 4,
                block_height: 4,
                validation: TexCompressionValidation::S3TC,
            },
            TexCompression {
                format: TexFormat::CompressedSrgbAlphaS3tcDxt3,
                bytes_per_block: 16,
                block_width: 4,
                block_height: 4,
                validation: TexCompressionValidation::S3TC,
            },
            TexCompression {
                format: TexFormat::CompressedSrgbAlphaS3tcDxt5,
                bytes_per_block: 16,
                block_width: 4,
                block_height: 4,
                validation: TexCompressionValidation::S3TC,
            },
        ]);
    }

    fn name() -> &'static str {
        "WEBGL_compressed_texture_s3tc_srgb"
    }
}
