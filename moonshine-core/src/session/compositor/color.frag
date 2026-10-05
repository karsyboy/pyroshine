#version 100
//_DEFINES_
#ifdef EXTERNAL
#extension GL_OES_EGL_image_external : require
#endif
precision highp float;
#ifdef EXTERNAL
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif
uniform float alpha;
varying vec2 v_coords;
#ifdef DEBUG_FLAGS
uniform float tint;
#endif

vec3 decode_srgb(vec3 v) {
    return mix(v / 12.92, pow(max((v + 0.055) / 1.055, vec3(0.0)), vec3(2.4)), step(vec3(0.04045), v));
}
vec3 encode_srgb(vec3 v) {
    v = clamp(v, 0.0, 1.0);
    return mix(v * 12.92, 1.055 * pow(v, vec3(1.0 / 2.4)) - 0.055, step(vec3(0.0031308), v));
}
vec3 encode_pq(vec3 nits) {
    vec3 p = pow(clamp(nits / 10000.0, 0.0, 1.0), vec3(0.1593017578125));
    return pow((0.8359375 + 18.8515625 * p) / (1.0 + 18.6875 * p), vec3(78.84375));
}
vec3 decode_pq(vec3 v) {
    vec3 p = pow(max(v, vec3(0.0)), vec3(1.0 / 78.84375));
    return 10000.0 * pow(max(p - 0.8359375, vec3(0.0)) / max(18.8515625 - 18.6875 * p, vec3(0.00001)), vec3(1.0 / 0.1593017578125));
}
vec3 to2020(vec3 v) {
    return mat3(0.627404,0.069097,0.016391, 0.329283,0.919540,0.088013, 0.043313,0.011362,0.895595) * v;
}
vec3 to709(vec3 v) {
    return mat3(1.660491,-0.124550,-0.018151, -0.587641,1.132900,-0.100579, -0.072850,-0.008349,1.118730) * v;
}
void main() {
    vec4 sample_value = texture2D(tex, v_coords);
#ifdef NO_ALPHA
    sample_value.a = 1.0;
#endif
    // Wayland's default alpha is premultiplied in the electrical encoding.
    vec3 rgb = sample_value.a > 0.0 ? sample_value.rgb / sample_value.a : vec3(0.0);
#if SOURCE == 1
    vec3 nits2020 = decode_pq(rgb);
#elif SOURCE == 2
    vec3 nits2020 = to2020(rgb * 80.0);
#elif SOURCE == 3
    vec3 nits2020 = to2020(pow(max(rgb, vec3(0.0)), vec3(2.2)) * 203.0);
#else
    vec3 nits2020 = to2020(decode_srgb(rgb) * 203.0);
#endif
#if TARGET == 1
    rgb = encode_pq(nits2020);
#else
    rgb = encode_srgb(to709(nits2020) / 203.0);
#endif
    gl_FragColor = vec4(rgb * sample_value.a, sample_value.a) * alpha;
#ifdef DEBUG_FLAGS
    if (tint == 1.0) gl_FragColor = vec4(0.0,0.2,0.0,0.2) + gl_FragColor * 0.8;
#endif
}
