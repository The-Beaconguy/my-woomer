#version 330

// Unit quad (0..1), drawn as a triangle strip.
layout(location = 0) in vec2 aPos;

// Same maths as raylib's Camera2D with zero offset/rotation:
//   screen = (world - target) * zoom      (all in framebuffer pixels, y down)
uniform vec2 target;
uniform float zoom;
uniform vec2  viewport;   // framebuffer size in pixels
uniform vec2  texSize;    // screenshot size in pixels
uniform float mirror;     // 0.0 or 1.0
uniform float flip;       // 0.0 or 1.0, this one is vertical

out vec2 fragTexCoord;
out vec4 fragColor;

void main()
{
    vec2 world = aPos * texSize;
    vec2 screen = (world - target) * zoom;
    gl_Position = vec4(screen.x / viewport.x * 2.0 - 1.0,
                       1.0 - screen.y / viewport.y * 2.0,
                       0.0, 1.0);
    fragTexCoord = vec2(mix(aPos.x, 1.0 - aPos.x, mirror),
                        mix(aPos.y, 1.0 - aPos.y, flip));

    fragColor = vec4(1.0);
}
