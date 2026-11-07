#version 300 es
// GlassDock - vertex shader: positions are given in surface device pixels (origin top-left, y down).
precision highp float;
in vec2 a_pos;          // surface device pixels, origin top-left, y down
uniform vec2 u_surface;
out vec2 v_px;
void main() {
    v_px = a_pos;
    vec2 ndc = a_pos / u_surface * 2.0 - 1.0;
    gl_Position = vec4(ndc.x, -ndc.y, 0.0, 1.0);
}
