/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
// skip-unless CARGO_FEATURE_WEBGL

/*
 * WebGL IDL definitions from the Khronos specification:
 * https://www.khronos.org/registry/webgl/extensions/WEBGL_compressed_texture_s3tc_srgb/
 */

[LegacyNoInterfaceObject, Exposed=Window]
interface WEBGLCompressedTextureS3TCSrgb {
    /* Compressed Texture Formats */
    const GLenum COMPRESSED_SRGB_S3TC_DXT1_EXT       = 0x8C4C;
    const GLenum COMPRESSED_SRGB_ALPHA_S3TC_DXT1_EXT = 0x8C4D;
    const GLenum COMPRESSED_SRGB_ALPHA_S3TC_DXT3_EXT = 0x8C4E;
    const GLenum COMPRESSED_SRGB_ALPHA_S3TC_DXT5_EXT = 0x8C4F;
}; // interface WEBGLCompressedTextureS3TCSrgb
