// CrossZoom (radial zoom blur crossfade), ported to WGSL for stream-engine.
// Original: gl-transitions "CrossZoom" — License: MIT, Author: rectalogic
// (https://gl-transitions.com/editor/CrossZoom, ported by gre from
// https://gist.github.com/rectalogic/b86b90161503a0023231, based on glfx.js zoomblur by Evan Wallace).
// MIT License: Permission is hereby granted, free of charge, to any person obtaining a copy of this
// software and associated documentation files, to deal in the Software without restriction,
// subject to including this notice. THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND.
//
// Built-in transition shader `zoomblur` (`shader = "zoomblur"`); settings and defaults in
// se_core::transitions::SHADERS: strength (0.4).
const PI: f32 = 3.141592653589793;

fn linear_ease(begin: f32, change: f32, duration: f32, time: f32) -> f32 {
    return change * time / duration + begin;
}

fn exponential_ease_in_out(begin: f32, change: f32, duration: f32, time_in: f32) -> f32 {
    if time_in == 0.0 {
        return begin;
    }
    if time_in == duration {
        return begin + change;
    }
    let time = time_in / (duration / 2.0);
    if time < 1.0 {
        return change / 2.0 * pow(2.0, 10.0 * (time - 1.0)) + begin;
    }
    return change / 2.0 * (-pow(2.0, -10.0 * (time - 1.0)) + 2.0) + begin;
}

fn sinusoidal_ease_in_out(begin: f32, change: f32, duration: f32, time: f32) -> f32 {
    return -change / 2.0 * (cos(PI * time / duration) - 1.0) + begin;
}

fn rand(co: vec2<f32>) -> f32 {
    return fract(sin(dot(co, vec2<f32>(12.9898, 78.233))) * 43758.5453);
}

fn cross_fade(uv: vec2<f32>, dissolve: f32) -> vec3<f32> {
    let a = textureSampleLevel(se_input, se_sampler, uv, 0.0).rgb;
    let b = textureSampleLevel(se_input_b, se_sampler, uv, 0.0).rgb;
    return mix(a, b, dissolve);
}

@fragment
fn fs(in: SeVsOut) -> @location(0) vec4<f32> {
    let progress = se.progress;
    let uv = in.uv;
    // center travels across the middle half of the image
    let center = vec2<f32>(linear_ease(0.25, 0.5, 1.0, progress), 0.5);
    let dissolve = exponential_ease_in_out(0.0, 1.0, 1.0, progress);
    // mirrored sinusoidal loop: 0 → strength → 0
    let strength = sinusoidal_ease_in_out(0.0, p_strength(), 0.5, progress);
    var color = vec3<f32>(0.0);
    var total = 0.0;
    let to_center = center - uv;
    // randomize the lookups to hide the fixed number of samples
    let offset = rand(uv);
    for (var t = 0.0; t <= 40.0; t = t + 1.0) {
        let percent = (t + offset) / 40.0;
        let weight = 4.0 * (percent - percent * percent);
        color = color + cross_fade(uv + to_center * percent * strength, dissolve) * weight;
        total = total + weight;
    }
    return vec4<f32>(color / total, 1.0);
}
