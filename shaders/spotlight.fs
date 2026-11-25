#version 330

// Input vertex attributes (from vertex shader)
in vec2 fragTexCoord;
in vec4 fragColor;

// Input uniform values
uniform sampler2D texture0;
uniform vec4 colDiffuse;

uniform vec4 spotlightTint;
uniform vec2 cursorPosition;
uniform float spotlightRadiusMultiplier;

// This is multiplied by spotlightRadiusMultiplier to obtain the radius in pixels
const int UNIT_RADIUS = 60;

// Output fragment color
out vec4 finalColor;


void main()
{
    vec4 texelColor = texture(texture0, fragTexCoord);

    float distanceToCursor = distance(gl_FragCoord.xy, vec2(cursorPosition.x, cursorPosition.y));

    float spotlightRadius = float(UNIT_RADIUS) * spotlightRadiusMultiplier;
    if (distanceToCursor > spotlightRadius)  {
        finalColor = (mix(texelColor, vec4(spotlightTint.rgb, 1.0), spotlightTint.a) * colDiffuse);
    } else {
        finalColor = (texelColor * colDiffuse);
    }
}
