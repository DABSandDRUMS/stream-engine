// Glitch: blocky RGB displacement crossfade.
// Adapted for stream-engine from gl-transitions "GlitchMemories" — License: MIT,
// Author: Gunnar Roth, based on work from natewave (https://gl-transitions.com/editor/GlitchMemories).
// MIT License: Permission is hereby granted, free of charge, to any person obtaining a copy of this
// software and associated documentation files, to deal in the Software without restriction,
// subject to including this notice. THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND.
// Changes: blocks are computed in pixels (as in natewave's original) with hashed noise instead of
// a noise texture, and the displacement envelope peaks mid-transition (strength × 4p(1−p)) so the
// shader also works as a glitch burst over a morph (kind = "combined", where A == B).
//
// Built-in transition shader `glitch` (`shader = "glitch"`); settings and defaults in
// se_core::transitions::SHADERS: strength (1.0), block (px, 16).
fn hash2(p: vec2<f32>) -> vec2<f32> {
    var q = vec2<f32>(dot(p, vec2<f32>(127.1, 311.7)), dot(p, vec2<f32>(269.5, 183.3)));
    return fract(sin(q) * 43758.5453);
}

fn sample_mix(uv: vec2<f32>, progress: f32) -> vec4<f32> {
    let c = clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0));
    let a = textureSampleLevel(se_input, se_sampler, c, 0.0);
    let b = textureSampleLevel(se_input_b, se_sampler, c, 0.0);
    return mix(a, b, progress);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let progress = se.progress;
    let p = in.uv;
    let block = floor(p * se.resolution / max(p_block(), 1.0));
    var uv_noise = block / 64.0;
    uv_noise = uv_noise + floor(vec2<f32>(progress) * vec2<f32>(1200.0, 3500.0)) / 64.0;
    let envelope = 4.0 * progress * (1.0 - progress) * p_strength();
    let dist = (hash2(uv_noise) - 0.5) * 0.3 * envelope;
    let red = p + dist * 0.2;
    let green = p + dist * 0.3;
    let blue = p + dist * 0.5;
    return vec4<f32>(sample_mix(red, progress).r, sample_mix(green, progress).g, sample_mix(blue, progress).b, 1.0);
}
